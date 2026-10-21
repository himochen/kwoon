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
    /// The run clone and the private run home: the only binds that may be writable.
    /// They come from the run directory, not from the builder, so that the mount check
    /// holds the layout to plan D5 and not only to what the builder declared.
    pub run_clone: Option<PathBuf>,
    pub run_home: Option<PathBuf>,
    /// The host's namespace identities, by name, to tell which ones the wrapper created.
    pub host_namespaces: Vec<(String, u64)>,
}

/// Mounts that bubblewrap itself makes writable: its root and `/tmp` tmpfs, and its
/// `/dev`. They hold nothing of the host's files.
const BUBBLEWRAP_WRITABLE: [&str; 3] = ["/", "/tmp", "/dev"];
/// Trees that must be bound, read-only, in every layout (plan D5, "Mounts").
const READ_ONLY_SYSTEM: [&str; 2] = ["/usr", "/etc"];
/// Variables that must never be set inside the wrapper, whoever declared them (plan D5:
/// the agent socket, the forge tokens, the session bus and the runtime directory).
pub const FORBIDDEN_VARIABLES: [&str; 8] = [
    "SSH_AUTH_SOCK",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "GIT_ASKPASS",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// The checks a full-layout report must hold, each exactly once.
pub const REQUIRED_CHECKS: [&str; 7] = [
    "files",
    "store-write",
    "mounts",
    "descriptors",
    "sockets",
    "environment",
    "namespaces",
];

/// The namespaces the wrapper must create (plan D5: `--unshare-all` plus the user and
/// PID namespaces named again; the mount namespace is bubblewrap's own).
pub const REQUIRED_NAMESPACES: [&str; 5] = ["user", "pid", "ipc", "uts", "mnt"];
/// The namespace `--unshare-all` only tries to create.
pub const OPTIONAL_NAMESPACES: [&str; 1] = ["cgroup"];

/// The identity of each namespace this process is in, by name.
pub fn namespace_identities() -> Result<Vec<(String, u64)>, String> {
    REQUIRED_NAMESPACES
        .iter()
        .chain(&OPTIONAL_NAMESPACES)
        .map(|name| {
            fs::metadata(format!("/proc/self/ns/{name}"))
                .map(|meta| (name.to_string(), meta.ino()))
                .map_err(|err| format!("cannot read the {name} namespace: {err}"))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub stage: Stage,
    pub pid: u32,
    pub checks: Vec<Check>,
    pub reachable_tcp: Vec<String>,
    pub reachable_abstract: Vec<String>,
    /// What the service scan could not observe. Non-empty means the lists above may be
    /// incomplete.
    pub reachable_errors: Vec<String>,
    /// The optional namespaces the wrapper actually created.
    pub optional_namespaces_created: Vec<String>,
}

impl Report {
    /// Why the checks do not form a complete report: a required check missing or
    /// repeated, or one kwoon does not know.
    pub fn incomplete(&self) -> Option<String> {
        let mut problems = Vec::new();
        for name in REQUIRED_CHECKS {
            match self.checks.iter().filter(|c| c.name == name).count() {
                1 => {}
                0 => problems.push(format!("missing check {name}")),
                _ => problems.push(format!("repeated check {name}")),
            }
        }
        for check in &self.checks {
            if !REQUIRED_CHECKS.contains(&check.name.as_str()) {
                problems.push(format!("unknown check {}", check.name));
            }
        }
        (!problems.is_empty()).then(|| problems.join("; "))
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
        reachable_errors: Vec::new(),
        optional_namespaces_created: Vec::new(),
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
        check_environment(request, &variable_names()),
    ];
    let (namespaces, created) = check_namespaces(request, namespace_identities());
    report.checks.push(namespaces);
    report.optional_namespaces_created = created;
    let tables = [
        (Path::new("/proc/net/tcp"), false),
        (Path::new("/proc/net/tcp6"), true),
    ];
    let (tcp, tcp_errors) = reachable_tcp(&tables);
    let (unix, unix_errors) = reachable_abstract(Path::new("/proc/net/unix"));
    report.reachable_tcp = tcp;
    report.reachable_abstract = unix;
    report.reachable_errors = [tcp_errors, unix_errors].concat();
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

/// Mounts: the mount table holds exactly the declared mounts, each with its access,
/// and it meets plan D5 on its own terms: `/usr` and `/etc` are read-only, and no bind
/// but the run clone and the run home is writable, whatever was declared.
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
    let writable_allowed = |point: &Path| {
        BUBBLEWRAP_WRITABLE.iter().any(|p| point == Path::new(p))
            || point.starts_with("/dev")
            || point.starts_with("/proc")
            || request.run_clone.as_deref() == Some(point)
            || request.run_home.as_deref() == Some(point)
    };
    for mount in mounts.iter().filter(|m| !m.read_only) {
        if !writable_allowed(&mount.point) {
            problems.push(format!(
                "{} is writable, but only the run clone and the run home may be",
                mount.point.display()
            ));
        }
    }
    for system in READ_ONLY_SYSTEM {
        match mounts.iter().find(|m| m.point == Path::new(system)) {
            None => problems.push(format!("{system} is not bound")),
            Some(mount) if !mount.read_only => problems.push(format!("{system} must be read-only")),
            Some(_) => {}
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
    // A socket that cannot be tried is not shown unreachable: it fails the check.
    if let Some(path) = &request.ssh_auth_sock {
        match connect_unix(&UnixTarget::Path(path.clone())) {
            Ok(false) => {}
            Ok(true) => problems.push(format!("SSH agent reachable at {path}")),
            Err(err) => problems.push(format!("cannot check SSH agent at {path}: {err}")),
        }
    }
    if let Some(address) = &request.dbus_address {
        match bus_addresses(address) {
            Ok(targets) => {
                for target in targets {
                    match connect_unix(&target) {
                        Ok(false) => {}
                        Ok(true) => {
                            problems.push(format!("session bus reachable at {}", target.describe()))
                        }
                        Err(err) => problems.push(format!(
                            "cannot check session bus at {}: {err}",
                            target.describe()
                        )),
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
/// deadline. "Would block" means a listener exists, so it counts as reachable. Only
/// the errors that mean nothing listens there count as unreachable; any other failure,
/// including one to create the socket, is an error: the target was not tried.
fn connect_unix(target: &UnixTarget) -> Result<bool, String> {
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
        return Err("the address does not fit a Unix socket address".into());
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
            return Err(format!(
                "cannot create a socket: {}",
                io::Error::last_os_error()
            ));
        }
        let rc = libc::connect(
            fd,
            (&raw const addr).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
        );
        let err = io::Error::last_os_error();
        libc::close(fd);
        if rc == 0 {
            return Ok(true);
        }
        match err.raw_os_error() {
            Some(libc::EAGAIN | libc::EINPROGRESS) => Ok(true),
            // A socket of another type (for example a datagram one) exists there, and a
            // run can reach it with a socket of that type.
            Some(libc::EPROTOTYPE) => Ok(true),
            Some(libc::ENOENT | libc::ECONNREFUSED | libc::ENOTDIR | libc::EACCES) => Ok(false),
            _ => Err(err.to_string()),
        }
    }
}

/// Namespaces: every required one differs from the host's. Returns the check and the
/// optional namespaces that were created.
fn check_namespaces(
    request: &Request,
    own: Result<Vec<(String, u64)>, String>,
) -> (Check, Vec<String>) {
    let own = match own {
        Ok(own) => own,
        Err(err) => return (check("namespaces", vec![err]), Vec::new()),
    };
    let created = |name: &str| -> Result<bool, String> {
        let find = |list: &[(String, u64)]| list.iter().find(|(n, _)| n == name).map(|(_, id)| *id);
        match (find(&own), find(&request.host_namespaces)) {
            (Some(inside), Some(host)) => Ok(inside != host),
            _ => Err(format!("no identity for the {name} namespace")),
        }
    };
    let mut problems = Vec::new();
    for name in REQUIRED_NAMESPACES {
        match created(name) {
            Ok(true) => {}
            Ok(false) => problems.push(format!("the {name} namespace is the host's")),
            Err(err) => problems.push(err),
        }
    }
    let mut optional = Vec::new();
    for name in OPTIONAL_NAMESPACES {
        match created(name) {
            Ok(true) => optional.push(name.to_string()),
            Ok(false) => {}
            Err(err) => problems.push(err),
        }
    }
    (check("namespaces", problems), optional)
}

fn variable_names() -> BTreeSet<String> {
    std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect()
}

/// Environment: it holds exactly the variables the builder set, and none of the
/// forbidden ones, even if the builder declared it.
fn check_environment(request: &Request, present: &BTreeSet<String>) -> Check {
    let expected: BTreeSet<&str> = request.env.iter().map(String::as_str).collect();
    let mut problems = Vec::new();
    for name in present {
        if FORBIDDEN_VARIABLES.contains(&name.as_str()) {
            problems.push(format!("forbidden variable {name}"));
        }
    }
    for name in present {
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

/// Loopback TCP ports in the shared network namespace that accept a connection, and
/// what could not be observed. A missing IPv6 table means the kernel has no IPv6, so
/// nothing can listen there; any other read failure is an error.
fn reachable_tcp(tables: &[(&Path, bool)]) -> (Vec<String>, Vec<String>) {
    let mut found = BTreeSet::new();
    let mut errors = Vec::new();
    for (table, v6) in tables {
        let text = match fs::read_to_string(table) {
            Ok(text) => text,
            Err(err) if *v6 && err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                errors.push(format!("cannot read {}: {err}", table.display()));
                continue;
            }
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port = fields
                .get(1)
                .and_then(|local| local.split_once(':'))
                .and_then(|(_, port)| u16::from_str_radix(port, 16).ok());
            let (Some(state), Some(port)) = (fields.get(3), port) else {
                errors.push(format!("unreadable line in {}", table.display()));
                continue;
            };
            // State 0A is LISTEN.
            if *state != "0A" {
                continue;
            }
            let addr: SocketAddr = if *v6 {
                ([0, 0, 0, 0, 0, 0, 0, 1], port).into()
            } else {
                ([127, 0, 0, 1], port).into()
            };
            match tcp_outcome(TcpStream::connect_timeout(
                &addr,
                Duration::from_millis(200),
            )) {
                Ok(true) => {
                    found.insert(addr.to_string());
                }
                Ok(false) => {}
                Err(err) => errors.push(format!("cannot try {addr}: {err}")),
            }
        }
    }
    (found.into_iter().collect(), errors)
}

/// Whether a connection attempt to a listening loopback port reached it. As for Unix
/// sockets, only a refusal means nothing listens there; a listener that answers late or
/// resets still listens. Any other failure means the port was not tried: an error.
fn tcp_outcome(result: io::Result<TcpStream>) -> Result<bool, String> {
    match result {
        Ok(_) => Ok(true),
        Err(err) => match err.kind() {
            io::ErrorKind::ConnectionRefused => Ok(false),
            io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted => Ok(true),
            _ => Err(err.to_string()),
        },
    }
}

/// Abstract Unix sockets in the shared network namespace that accept a connection, and
/// what could not be observed.
fn reachable_abstract(table: &Path) -> (Vec<String>, Vec<String>) {
    let mut found = BTreeSet::new();
    let mut errors = Vec::new();
    let text = match fs::read_to_string(table) {
        Ok(text) => text,
        Err(err) => {
            return (
                Vec::new(),
                vec![format!("cannot read {}: {err}", table.display())],
            );
        }
    };
    for line in text.lines().skip(1) {
        let Some(name) = line
            .split_whitespace()
            .nth(7)
            .and_then(|p| p.strip_prefix('@'))
        else {
            continue;
        };
        match connect_unix(&UnixTarget::Abstract(name.to_string())) {
            Ok(true) => {
                found.insert(format!("@{name}"));
            }
            Ok(false) => {}
            Err(err) => errors.push(format!("cannot try @{name}: {err}")),
        }
    }
    (found.into_iter().collect(), errors)
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
            assert_eq!(connect_unix(&target), Ok(true));
        }
        assert!(start.elapsed() < Duration::from_secs(2));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert_eq!(
            connect_unix(&UnixTarget::Path(path.display().to_string())),
            Ok(true)
        );
        assert_eq!(
            connect_unix(&UnixTarget::Path(
                dir.path().join("absent").display().to_string()
            )),
            Ok(false)
        );
        assert_eq!(
            connect_unix(&UnixTarget::Abstract("kwoon-probe-unit-absent".into())),
            Ok(false)
        );
    }

    #[test]
    fn a_socket_that_cannot_be_tried_is_an_error_not_unreachable() {
        // Review R1, finding 4: an address no socket can have was reported unreachable.
        let long = format!("/tmp/{}", "x".repeat(200));
        assert!(connect_unix(&UnixTarget::Path(long.clone())).is_err());
        let request = Request {
            ssh_auth_sock: Some(long),
            ..empty_request()
        };
        let check = check_sockets(&request);
        assert!(!check.ok);
        assert!(
            check
                .detail
                .starts_with("cannot check SSH agent at /tmp/xxx")
        );
    }

    fn empty_request() -> Request {
        Request {
            stage: Stage::Full,
            protected: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            state_dir: None,
            ssh_auth_sock: None,
            dbus_address: None,
            run_clone: Some("/runs/1/clone".into()),
            run_home: Some("/runs/1/home".into()),
            host_namespaces: Vec::new(),
        }
    }

    #[test]
    fn a_required_namespace_shared_with_the_host_fails() {
        let names = ["user", "pid", "ipc", "uts", "mnt", "cgroup"];
        let host: Vec<(String, u64)> = names.iter().map(|n| (n.to_string(), 1)).collect();
        let request = Request {
            host_namespaces: host.clone(),
            ..empty_request()
        };
        // All new: passes, and the optional cgroup namespace is reported created.
        let all_new: Vec<(String, u64)> = names.iter().map(|n| (n.to_string(), 2)).collect();
        let (check, created) = check_namespaces(&request, Ok(all_new.clone()));
        assert!(check.ok, "{}", check.detail);
        assert_eq!(created, ["cgroup"]);
        // The IPC namespace is the host's: fails. The cgroup one too: reported absent.
        let mut shared = all_new;
        for entry in &mut shared {
            if entry.0 == "ipc" || entry.0 == "cgroup" {
                entry.1 = 1;
            }
        }
        let (check, created) = check_namespaces(&request, Ok(shared));
        assert_eq!(check.detail, "the ipc namespace is the host's");
        assert!(created.is_empty());
        // No evidence: fails.
        let (check, _) = check_namespaces(&request, Err("unreadable".into()));
        assert!(!check.ok);
    }

    fn mount(point: &str, read_only: bool) -> Mount {
        Mount {
            point: point.into(),
            read_only,
        }
    }

    fn declared(point: &str, access: Access) -> DeclaredMount {
        DeclaredMount {
            mount_point: point.into(),
            access,
            optional: false,
        }
    }

    #[test]
    fn the_mount_policy_does_not_take_the_builders_word() {
        // Review R1, finding 6: every mount below is declared as observed.
        let layout = |usr_read_only: bool, opt_read_only: bool| {
            let mounts = vec![
                mount("/", false),
                mount("/usr", usr_read_only),
                mount("/etc", true),
                mount("/opt/harness", opt_read_only),
                mount("/runs/1/clone", false),
                mount("/runs/1/home", false),
            ];
            let access = |ro: bool| {
                if ro {
                    Access::ReadOnly
                } else {
                    Access::Writable
                }
            };
            let request = Request {
                mounts: vec![
                    declared("/", Access::Any),
                    declared("/usr", access(usr_read_only)),
                    declared("/etc", Access::ReadOnly),
                    declared("/opt/harness", access(opt_read_only)),
                    declared("/runs/1/clone", Access::Writable),
                    declared("/runs/1/home", Access::Writable),
                ],
                ..empty_request()
            };
            check_mounts(&request, &Ok(mounts))
        };
        assert!(layout(true, true).ok, "{}", layout(true, true).detail);
        let writable_usr = layout(false, true);
        assert!(!writable_usr.ok);
        assert!(writable_usr.detail.contains("/usr must be read-only"));
        let writable_opt = layout(true, false);
        assert_eq!(
            writable_opt.detail,
            "/opt/harness is writable, but only the run clone and the run home may be"
        );
        // Without /usr at all, the layout fails even though nothing declared it.
        let request = Request {
            mounts: vec![
                declared("/", Access::Any),
                declared("/etc", Access::ReadOnly),
            ],
            ..empty_request()
        };
        let check = check_mounts(&request, &Ok(vec![mount("/", false), mount("/etc", true)]));
        assert_eq!(check.detail, "/usr is not bound");
    }

    #[test]
    fn a_forbidden_variable_fails_although_declared() {
        let request = Request {
            env: vec!["HOME".into(), "GITHUB_TOKEN".into()],
            ..empty_request()
        };
        let present: BTreeSet<String> = ["HOME", "GITHUB_TOKEN"].map(String::from).into();
        let check = check_environment(&request, &present);
        assert_eq!(check.detail, "forbidden variable GITHUB_TOKEN");
    }

    #[test]
    fn a_tcp_port_that_cannot_be_tried_is_an_error_not_unreachable() {
        // Review R2: connect_timeout(..).is_ok() read every failure as "unreachable".
        let os = io::Error::from_raw_os_error;
        assert_eq!(tcp_outcome(Err(os(libc::ECONNREFUSED))), Ok(false));
        // A listener that does not answer in time still listens there.
        assert_eq!(tcp_outcome(Err(io::ErrorKind::TimedOut.into())), Ok(true));
        assert!(tcp_outcome(Err(os(libc::EMFILE))).is_err());
        assert!(tcp_outcome(Err(os(libc::EADDRNOTAVAIL))).is_err());
        assert!(tcp_outcome(Err(os(libc::EPERM))).is_err());
    }

    #[test]
    fn unreadable_service_tables_are_reported_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        let (found, errors) = reachable_tcp(&[(&absent, false)]);
        assert!(found.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        // A missing IPv6 table means no IPv6, and no error.
        assert_eq!(reachable_tcp(&[(&absent, true)]).1, Vec::<String>::new());
        let (_, errors) = reachable_abstract(&absent);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let garbled = dir.path().join("tcp");
        fs::write(&garbled, "header\nnot a table line\n").unwrap();
        assert_eq!(reachable_tcp(&[(&garbled, false)]).1.len(), 1);
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
