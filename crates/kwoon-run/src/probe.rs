//! The probe: the static `kwoon` executable, run as `kwoon __probe <request>` inside
//! the wrapper. It reports what it can see; the host side judges the report.
//!
//! Its report names paths, inode numbers, ports, and socket names only. It never reads
//! the contents of anything it finds.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::builder::{Access, DeclaredMount};

/// Where the probe executable is bound inside the sandbox.
pub const PROBE_PATH: &str = "/.kwoon-probe";
/// Where the probe's request file is bound inside the sandbox.
pub const PROBE_REQUEST_PATH: &str = "/.kwoon-probe-request";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Namespace setup: the probe only reports that it started, and its pid.
    Namespace,
    /// Filesystem setup: the complete production layout; every check runs.
    Full,
}

/// An object the run must not reach, by identity and by its host path, as the host
/// sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protected {
    pub dev: u64,
    pub ino: u64,
    pub path: PathBuf,
    /// The link count of a file; 0 for a directory.
    pub links: u64,
    /// The (device, inode) of each ancestor directory on the host, up to `/`. Empty for
    /// an object under a protected directory, whose ancestors are covered by that
    /// directory's own entry.
    pub ancestors: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub stage: Stage,
    pub protected: Vec<Protected>,
    pub mounts: Vec<DeclaredMount>,
    pub env: Vec<String>,
    /// The state directory; creating a file in it must fail (G4).
    pub state_dir: Option<PathBuf>,
    /// The operator's `SSH_AUTH_SOCK` and `DBUS_SESSION_BUS_ADDRESS`, if set.
    pub ssh_auth_sock: Option<String>,
    pub dbus_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub stage: Stage,
    pub pid: u32,
    pub checks: Vec<Check>,
    pub reachable_tcp: Vec<String>,
    pub reachable_abstract: Vec<String>,
}

impl Report {
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }
}

/// Runs the probe for `request` in the current process.
pub fn run(request: &Request) -> Report {
    let mut report = Report {
        stage: request.stage,
        pid: std::process::id(),
        checks: Vec::new(),
        reachable_tcp: Vec::new(),
        reachable_abstract: Vec::new(),
    };
    if request.stage == Stage::Namespace {
        return report;
    }
    let mounts = read_mountinfo();
    report.checks = vec![
        check_files(request, mounts.as_ref().ok()),
        check_store_write(request),
        check_mounts(request, &mounts),
        check_descriptors(),
        check_sockets(request),
        check_environment(request),
    ];
    report.reachable_tcp = reachable_tcp();
    report.reachable_abstract = reachable_abstract();
    report
}

fn check(name: &str, problems: Vec<String>) -> Check {
    Check {
        name: name.into(),
        ok: problems.is_empty(),
        detail: problems.join("; "),
    }
}

#[derive(Debug, Clone)]
struct Mount {
    point: PathBuf,
    read_only: bool,
}

fn read_mountinfo() -> io::Result<Vec<Mount>> {
    let file = fs::File::open("/proc/self/mountinfo")?;
    let mut mounts = Vec::new();
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short mountinfo line",
            ));
        }
        mounts.push(Mount {
            point: PathBuf::from(unescape(fields[4])),
            read_only: fields[5].split(',').any(|opt| opt == "ro"),
        });
    }
    Ok(mounts)
}

/// mountinfo escapes space, tab, newline, and backslash as three octal digits.
fn unescape(field: &str) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let digits = std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or("0");
            out.push(u8::from_str_radix(digits, 8).unwrap_or(0));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

/// Files: no protected object is reachable, directly by its host path or through an
/// alias in any mounted tree, `/usr` and `/etc` included.
///
/// Instead of walking every file of every tree, which does not fit the stage deadline
/// on hosts with a large `/usr`, the check rests on how an alias can arise. The host
/// lists every protected object, including everything under a protected directory
/// (`doctor::protected_paths`), so these cases cover all of them:
/// - a hard link: a file has another link only if its link count is above 1, so such a
///   file fails the check wherever the other link is;
/// - a bind mount: a mount exposes exactly the subtree under its root, so a protected
///   object is reachable through a mount only if the mount's root is the object itself
///   or one of its ancestors; directories cannot be hard-linked;
/// - a symbolic link: it resolves inside the wrapper, so it reaches only what the two
///   cases above already make reachable.
fn check_files(request: &Request, mounts: Option<&Vec<Mount>>) -> Check {
    let mut problems = Vec::new();
    for item in &request.protected {
        if let Ok(meta) = fs::metadata(&item.path)
            && (meta.dev(), meta.ino()) == (item.dev, item.ino)
        {
            problems.push(format!(
                "{} is reachable at its own path",
                item.path.display()
            ));
        }
        if item.links > 1 {
            problems.push(format!(
                "{} has {} links, and another one may be reachable",
                item.path.display(),
                item.links
            ));
        }
    }
    let Some(mounts) = mounts else {
        problems.push("mount table unreadable; mounts not checked".into());
        return check("files", problems);
    };
    for mount in mounts.iter().filter(|m| !m.point.starts_with("/proc")) {
        let Ok(meta) = fs::metadata(&mount.point) else {
            problems.push(format!("cannot inspect mount {}", mount.point.display()));
            continue;
        };
        let root = (meta.dev(), meta.ino());
        for item in &request.protected {
            if root == (item.dev, item.ino) || item.ancestors.contains(&root) {
                problems.push(format!(
                    "mount {} exposes {}",
                    mount.point.display(),
                    item.path.display()
                ));
            }
        }
    }
    check("files", problems)
}

/// G4: creating a file in the state directory fails.
fn check_store_write(request: &Request) -> Check {
    let mut problems = Vec::new();
    if let Some(dir) = &request.state_dir {
        let target = dir.join(".kwoon-probe-write");
        if fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .is_ok()
        {
            let _ = fs::remove_file(&target);
            problems.push(format!("created a file in {}", dir.display()));
        }
    }
    check("store-write", problems)
}

/// Mounts: the mount table holds exactly the declared mounts, each with its access.
fn check_mounts(request: &Request, mounts: &io::Result<Vec<Mount>>) -> Check {
    let mounts = match mounts {
        Ok(mounts) => mounts,
        Err(err) => return check("mounts", vec![format!("cannot read mountinfo: {err}")]),
    };
    let mut problems = Vec::new();
    for mount in mounts {
        match request.mounts.iter().find(|d| d.mount_point == mount.point) {
            None => problems.push(format!("undeclared mount {}", mount.point.display())),
            Some(declared) => match declared.access {
                Access::ReadOnly if !mount.read_only => {
                    problems.push(format!("{} is writable", mount.point.display()))
                }
                Access::Writable if mount.read_only => {
                    problems.push(format!("{} is read-only", mount.point.display()))
                }
                _ => {}
            },
        }
    }
    let present: HashSet<&Path> = mounts.iter().map(|m| m.point.as_path()).collect();
    for declared in request.mounts.iter().filter(|d| !d.optional) {
        if !present.contains(declared.mount_point.as_path()) {
            problems.push(format!(
                "declared mount {} is missing",
                declared.mount_point.display()
            ));
        }
    }
    check("mounts", problems)
}

/// Descriptors: only 0, 1, and 2 are open.
fn check_descriptors() -> Check {
    let names: Vec<String> = match fs::read_dir("/proc/self/fd") {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(err) => {
            return check(
                "descriptors",
                vec![format!("cannot list /proc/self/fd: {err}")],
            );
        }
    };
    // The listing's own descriptor is closed by now; anything still open was inherited.
    let open: BTreeSet<i32> = names
        .iter()
        .filter_map(|name| name.parse::<i32>().ok())
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
        .collect();
    let extra: Vec<String> = open
        .iter()
        .filter(|fd| **fd > 2)
        .map(|fd| fd.to_string())
        .collect();
    let mut problems = Vec::new();
    if !extra.is_empty() {
        problems.push(format!("extra descriptors open: {}", extra.join(", ")));
    }
    for fd in 0..=2 {
        if !open.contains(&fd) {
            problems.push(format!("descriptor {fd} is not open"));
        }
    }
    check("descriptors", problems)
}

/// Sockets: the operator's SSH agent and session bus cannot be reached.
fn check_sockets(request: &Request) -> Check {
    let mut problems = Vec::new();
    if let Some(path) = &request.ssh_auth_sock
        && connect_unix(&UnixTarget::Path(path.clone()))
    {
        problems.push(format!("SSH agent reachable at {path}"));
    }
    if let Some(address) = &request.dbus_address {
        match bus_addresses(address) {
            Ok(targets) => {
                for target in targets {
                    if connect_unix(&target) {
                        problems.push(format!("session bus reachable at {}", target.describe()));
                    }
                }
            }
            Err(unsupported) => {
                problems.push(format!("cannot check session bus address: {unsupported}"))
            }
        }
    }
    check("sockets", problems)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnixTarget {
    Path(String),
    Abstract(String),
}

impl UnixTarget {
    fn describe(&self) -> String {
        match self {
            UnixTarget::Path(path) => path.clone(),
            UnixTarget::Abstract(name) => format!("@{name}"),
        }
    }
}

/// The Unix sockets a D-Bus address names. Forms kwoon cannot check are an error, so
/// the check fails closed rather than passing on an address it did not try.
pub fn bus_addresses(address: &str) -> Result<Vec<UnixTarget>, String> {
    let mut targets = Vec::new();
    for entry in address.split(';').filter(|e| !e.is_empty()) {
        let Some(params) = entry.strip_prefix("unix:") else {
            return Err(format!("transport of {entry:?}"));
        };
        let mut target = None;
        for pair in params.split(',') {
            match pair.split_once('=') {
                Some(("path", value)) => target = Some(UnixTarget::Path(dbus_unescape(value))),
                Some(("abstract", value)) => {
                    target = Some(UnixTarget::Abstract(dbus_unescape(value)))
                }
                Some(("guid", _)) => {}
                _ => return Err(format!("parameter {pair:?}")),
            }
        }
        targets.push(target.ok_or_else(|| format!("no socket in {entry:?}"))?);
    }
    Ok(targets)
}

fn dbus_unescape(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Some(byte) = value
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a Unix stream listener answers at `target`. The connect does not block: a
/// listener whose accept queue is full would otherwise hold the probe until its
/// deadline. "Would block" means a listener exists, so it counts as reachable.
fn connect_unix(target: &UnixTarget) -> bool {
    let (bytes, abstract_name) = match target {
        UnixTarget::Path(path) => (path.as_bytes(), false),
        UnixTarget::Abstract(name) => (name.as_bytes(), true),
    };
    // SAFETY: zeroed sockaddr_un is valid; the copy below stays within sun_path.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let offset = usize::from(abstract_name);
    if bytes.is_empty() || offset + bytes.len() > addr.sun_path.len() - usize::from(!abstract_name)
    {
        return false;
    }
    for (slot, byte) in addr.sun_path[offset..].iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + offset + bytes.len();
    // SAFETY: a fresh socket, closed below; addr is initialized for len bytes.
    unsafe {
        let fd = libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        );
        if fd < 0 {
            return false;
        }
        let rc = libc::connect(
            fd,
            (&raw const addr).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
        );
        let errno = io::Error::last_os_error().raw_os_error();
        libc::close(fd);
        rc == 0 || matches!(errno, Some(libc::EAGAIN) | Some(libc::EINPROGRESS))
    }
}

/// Environment: it holds exactly the variables the builder set.
fn check_environment(request: &Request) -> Check {
    let expected: BTreeSet<&str> = request.env.iter().map(String::as_str).collect();
    let present: BTreeSet<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    let mut problems = Vec::new();
    for name in &present {
        if !expected.contains(name.as_str()) {
            problems.push(format!("unexpected variable {name}"));
        }
    }
    for name in &expected {
        if !present.contains(*name) {
            problems.push(format!("missing variable {name}"));
        }
    }
    check("environment", problems)
}

/// Loopback TCP ports in the shared network namespace that accept a connection.
fn reachable_tcp() -> Vec<String> {
    let mut found = BTreeSet::new();
    for (table, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // State 0A is LISTEN.
            if fields.len() < 4 || fields[3] != "0A" {
                continue;
            }
            let Some((_, port_hex)) = fields[1].split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            let addr: SocketAddr = if v6 {
                ([0, 0, 0, 0, 0, 0, 0, 1], port).into()
            } else {
                ([127, 0, 0, 1], port).into()
            };
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                found.insert(addr.to_string());
            }
        }
    }
    found.into_iter().collect()
}

/// Abstract Unix sockets in the shared network namespace that accept a connection.
fn reachable_abstract() -> Vec<String> {
    let mut found = BTreeSet::new();
    let Ok(text) = fs::read_to_string("/proc/net/unix") else {
        return Vec::new();
    };
    for line in text.lines().skip(1) {
        let Some(path) = line.split_whitespace().nth(7) else {
            continue;
        };
        if let Some(name) = path.strip_prefix('@')
            && connect_unix(&UnixTarget::Abstract(name.to_string()))
        {
            found.insert(format!("@{name}"));
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr as UnixAddr, UnixListener};
    use std::time::Instant;

    #[test]
    fn a_listener_with_a_full_queue_is_reachable_without_blocking() {
        let name = format!("kwoon-probe-unit-{}", std::process::id());
        let listener =
            UnixListener::bind_addr(&UnixAddr::from_abstract_name(&name).unwrap()).unwrap();
        // SAFETY: shrinks the accept queue of a socket we own.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let target = UnixTarget::Abstract(name);
        let start = Instant::now();
        for _ in 0..8 {
            assert!(connect_unix(&target));
        }
        assert!(start.elapsed() < Duration::from_secs(2));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(connect_unix(&UnixTarget::Path(path.display().to_string())));
        assert!(!connect_unix(&UnixTarget::Path(
            dir.path().join("absent").display().to_string()
        )));
        assert!(!connect_unix(&UnixTarget::Abstract(
            "kwoon-probe-unit-absent".into()
        )));
    }

    #[test]
    fn parses_bus_addresses_and_refuses_unknown_forms() {
        assert_eq!(
            bus_addresses("unix:path=/run/user/1000/bus,guid=abc").unwrap(),
            [UnixTarget::Path("/run/user/1000/bus".into())]
        );
        assert_eq!(
            bus_addresses("unix:abstract=/tmp/dbus-x;unix:path=/a%20b").unwrap(),
            [
                UnixTarget::Abstract("/tmp/dbus-x".into()),
                UnixTarget::Path("/a b".into())
            ]
        );
        assert!(bus_addresses("tcp:host=localhost,port=1").is_err());
        assert!(bus_addresses("unix:tmpdir=/tmp").is_err());
    }

    #[test]
    fn unescapes_mountinfo_fields() {
        assert_eq!(unescape(r"/a\040b"), OsString::from("/a b"));
        assert_eq!(unescape("/plain"), OsString::from("/plain"));
    }
}
