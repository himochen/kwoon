//! Starting a wrapper and keeping its process tree under control.
//!
//! The wrapper child gets exactly three descriptors: stdin from `/dev/null`, and pipes
//! for stdout and stderr. Every other descriptor kwoon holds is close-on-exec from
//! [`mark_descriptors_cloexec`]. The child leads its own process group; on its deadline
//! the group is killed, and bubblewrap's `--die-with-parent` and PID namespace take the
//! rest of the tree down with it.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

/// Runs `argv` with its deadline.
pub fn run(argv: &[OsString], deadline: Duration) -> io::Result<Outcome> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    let ending = wait_until(&mut child, Instant::now() + deadline)?;
    let wait_for = |rx: mpsc::Receiver<Vec<u8>>| rx.recv_timeout(DRAIN_GRACE).ok();
    let (out, err) = (wait_for(stdout), wait_for(stderr));
    Ok(Outcome {
        ending,
        pipes_closed: out.is_some() && err.is_some(),
        stdout: out.unwrap_or_default(),
        stderr: err.unwrap_or_default(),
    })
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
