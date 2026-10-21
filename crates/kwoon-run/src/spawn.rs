//! Starting a wrapper and keeping its process tree under control.
//!
//! bubblewrap's options reach it through `--args <fd>`, NUL-separated on a pipe, and
//! never through its argv, which any local user can read in `/proc/<pid>/cmdline`: the
//! options carry the harness's provider credential (maintainer decision on #20, a
//! wording fix to plan D5). So bubblewrap gets four descriptors: stdin from
//! `/dev/null`, pipes for stdout and stderr, and the read end of the options pipe,
//! which it reads before it starts the sandboxed process. That process gets exactly
//! the first three; the probe's descriptor check shows it. Every other descriptor kwoon
//! holds is close-on-exec from [`mark_descriptors_cloexec`]. The child leads its own
//! process group; on its deadline the group is killed, and bubblewrap's
//! `--die-with-parent` and PID namespace take the rest of the tree down with it.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::builder::Invocation;

/// Output beyond this many bytes per stream is read and dropped, so a run cannot
/// exhaust kwoon's memory through its stdout.
const OUTPUT_CAP: usize = 4 << 20;
/// After a kill, how long to wait for the pipes to close before giving up on them.
const DRAIN_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

/// Marks every descriptor above 2 close-on-exec, so nothing kwoon inherited reaches a
/// child. Call it first thing in `main`.
pub fn mark_descriptors_cloexec() -> io::Result<()> {
    // SAFETY: close_range only changes descriptor flags; it takes no pointers.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    Signaled(i32),
    /// The deadline passed; the process group was killed.
    TimedOut,
}

#[derive(Debug)]
pub struct Outcome {
    pub ending: Ending,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// False when a pipe was still held open after the kill: some process of the tree
    /// survived it.
    pub pipes_closed: bool,
}

/// Starts bubblewrap as `invocation` says and waits for it, up to `deadline`.
pub fn run(invocation: &Invocation, deadline: Duration) -> io::Result<Outcome> {
    // A NUL inside an option would end it early on the pipe, and bubblewrap would take
    // the rest as further options. argv refused it at spawn; refuse it here. The value
    // stays out of the error, which kwoon prints.
    if invocation.options.iter().any(|o| o.as_bytes().contains(&0)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a bubblewrap option contains a NUL byte",
        ));
    }
    let (read, write) = pipe()?;
    let fd = read.as_raw_fd();
    let mut command = Command::new(&invocation.program);
    command
        .arg("--args")
        .arg(fd.to_string())
        .args(&invocation.command)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: fcntl is async-signal-safe; it only clears close-on-exec on the read end,
    // under its own number, so it replaces none of the child's other descriptors.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(read);
    // Written while bubblewrap reads, so options longer than the pipe's buffer do not
    // block; a bubblewrap that exits early only makes the write fail.
    let mut stream = Vec::new();
    for option in &invocation.options {
        stream.extend_from_slice(option.as_bytes());
        stream.push(0);
    }
    let writer = thread::spawn(move || {
        let _ = File::from(write).write_all(&stream);
    });
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    let ending = wait_until(&mut child, Instant::now() + deadline)?;
    let _ = writer.join();
    let wait_for = |rx: mpsc::Receiver<Vec<u8>>| rx.recv_timeout(DRAIN_GRACE).ok();
    let (out, err) = (wait_for(stdout), wait_for(stderr));
    Ok(Outcome {
        ending,
        pipes_closed: out.is_some() && err.is_some(),
        stdout: out.unwrap_or_default(),
        stderr: err.unwrap_or_default(),
    })
}

/// A pipe whose ends are both close-on-exec.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills both descriptors, which are owned below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Ending> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(match status.code() {
                Some(code) => Ending::Exited(code),
                None => Ending::Signaled(status.signal().unwrap_or(0)),
            });
        }
        if Instant::now() >= deadline {
            kill_group(child);
            child.wait()?;
            return Ok(Ending::TimedOut);
        }
        thread::sleep(POLL);
    }
}

fn kill_group(child: &Child) {
    let pgid = child.id() as libc::pid_t;
    // SAFETY: kill takes no pointers. The group is the child's own (process_group(0)),
    // and the child has not been reaped yet, so the id cannot have been reused.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = OUTPUT_CAP.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn an_option_containing_nul_is_refused_before_anything_starts() {
        // Standing in for bubblewrap, a program that leaves a mark when it starts.
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("bwrap");
        let mark = dir.path().join("started");
        fs::write(&program, format!("#!/bin/sh\n: > '{}'\n", mark.display())).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let invocation = Invocation {
            program,
            options: vec!["--setenv".into(), "K".into(), "x\0--bind\0/\0/".into()],
            command: vec!["/probe".into()],
        };
        let err = run(&invocation, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!err.to_string().contains("--bind"), "{err}");
        assert!(!mark.exists(), "the program was started");
    }
}
