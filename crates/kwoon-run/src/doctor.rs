//! The `kwoon doctor` items for the wrapper (plan D5). Every item uses the production
//! builder. An item whose test did not run is `unknown`, never `verified` (G3).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::builder::Sandbox;
use crate::contain;
use crate::harness::Kind;
use crate::home::{self, RunDir, SeedFile};
use crate::probe::{self, PROBE_PATH, PROBE_REQUEST_PATH, Protected, Report, Request, Stage};
use crate::spawn::{self, Ending, Outcome};

/// Each bubblewrap setup stage has this deadline (plan D6, "Deadlines").
pub const STAGE_DEADLINE: Duration = Duration::from_secs(30);
/// A session log larger than this is refused rather than read.
const LOG_LIMIT: u64 = 64 << 20;
const PROMPT: &str = "Reply with the single word: ready";

/// What the host side knows, from host settings and the operator's environment.
pub struct Host {
    pub bwrap: PathBuf,
    /// The static kwoon executable, bound read-only as the probe.
    pub probe: PathBuf,
    pub runs_dir: PathBuf,
    pub state_dir: PathBuf,
    pub protected: Vec<Protected>,
    /// Each forge credential's id and bytes, which must never be seeded into a run.
    pub forge_secrets: Vec<(String, Vec<u8>)>,
    pub ssh_auth_sock: Option<String>,
    pub dbus_address: Option<String>,
    pub lang: String,
}

pub struct Harness {
    pub kind: Kind,
    pub executable: PathBuf,
    /// Host paths the harness needs, bound read-only at the same path.
    pub ro_binds: Vec<PathBuf>,
    /// Variables from host settings.
    pub env: Vec<(String, String)>,
    pub credential: Option<Credential>,
    pub timeout: Duration,
}

/// The harness's provider credential, already read from its source on the host.
pub enum Credential {
    /// Set inside the wrapper as this variable.
    Env { name: String, value: String },
    /// Copied into the run home at the harness's credential path.
    File { bytes: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Verified,
    NotProvided,
    Unknown,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Verified => "verified",
            Status::NotProvided => "not provided",
            Status::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

fn item(name: &'static str, status: Status, detail: impl Into<String>) -> Item {
    Item {
        name,
        status,
        detail: detail.into(),
    }
}

/// Runs every wrapper item for one harness.
pub fn check(host: &Host, harness: &Harness) -> Vec<Item> {
    let run = match RunDir::create(&host.runs_dir, &format!("doctor-{}", harness.kind.name())) {
        Ok(run) => run,
        Err(err) => {
            let why = format!("cannot create a run directory: {err}");
            return [
                "wrapper-starts",
                "wrapper-hides-state",
                "wrapper-reachable-services",
                "harness-config-clean",
                "harness-starts-in-wrapper",
            ]
            .map(|name| item(name, Status::Unknown, why.clone()))
            .to_vec();
        }
    };
    let items = check_in(host, harness, &run);
    let _ = run.remove();
    items
}

fn check_in(host: &Host, harness: &Harness, run: &RunDir) -> Vec<Item> {
    let (seed_files, env) = seed_for(harness);
    let config_clean = check_config(host, &seed_files, &env);
    if config_clean.status == Status::Verified
        && let Err(err) = home::seed(&run.home, &seed_files)
    {
        return finish(
            Stages::Failed(format!("cannot seed the run home: {err}")),
            None,
            item(
                "harness-config-clean",
                Status::Unknown,
                format!("seeding failed: {err}"),
            ),
            None,
        );
    }

    // Stage 1: the namespace and process options, with only the probe in an otherwise
    // empty filesystem.
    let request_file = run.root.join("probe-request-1.json");
    let stage1 = probe_sandbox(Sandbox::new(&host.bwrap).share_net(), host, &request_file);
    let request = Request {
        stage: Stage::Namespace,
        protected: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        state_dir: None,
        ssh_auth_sock: None,
        dbus_address: None,
        run_clone: None,
        run_home: None,
        host_namespaces: Vec::new(),
        host_devices: Vec::new(),
    };
    if let Err(why) =
        run_probe(&stage1, &request, &request_file).and_then(|report| expect_pid_namespace(&report))
    {
        // Namespace-creation failures are reported apart from probe-execution failures.
        let why = if why.starts_with("namespace creation failed")
            || why.starts_with("probe execution failed")
        {
            why
        } else {
            format!("probe execution failed: {why}")
        };
        let why = format!("stage 1 (namespace setup): {why}");
        return finish(Stages::Failed(why), None, config_clean, None);
    }

    // Stages 2 and 3 carry the seeded variables on bubblewrap's command line, so
    // neither is built when the scan refused one of them.
    if config_clean.status != Status::Verified {
        let why = "stages 2 and 3 not run: harness-config-clean did not pass";
        return finish(Stages::Withheld(why), None, config_clean, None);
    }

    // Stage 2: the probe with the complete production layout.
    let layout = run_layout(host, harness, run, &env);
    let request_file = run.root.join("probe-request-2.json");
    let stage2 = probe_sandbox(layout.clone(), host, &request_file);
    let request = Request {
        stage: Stage::Full,
        protected: host.protected.clone(),
        mounts: stage2.declared_mounts(),
        env: stage2.declared_env(),
        state_dir: Some(host.state_dir.clone()),
        ssh_auth_sock: host.ssh_auth_sock.clone(),
        dbus_address: host.dbus_address.clone(),
        run_clone: Some(run.clone.clone()),
        run_home: Some(run.home.clone()),
        host_namespaces: match probe::namespace_identities() {
            Ok(ids) => ids,
            Err(err) => {
                let why = format!("stage 2 (filesystem setup): {err}");
                return finish(Stages::Failed(why), None, config_clean, None);
            }
        },
        host_devices: match probe::mount_devices() {
            Ok(devices) => devices,
            Err(err) => {
                let why = format!("stage 2 (filesystem setup): {err}");
                return finish(Stages::Failed(why), None, config_clean, None);
            }
        },
    };
    let report = match run_probe(&stage2, &request, &request_file)
        .and_then(|r| expect_pid_namespace(&r).map(|_| r))
    {
        Ok(report) => report,
        Err(why) => {
            let why = format!("stage 2 (filesystem setup): {why}");
            return finish(Stages::Failed(why), None, config_clean, None);
        }
    };

    // Stage 3: the harness itself, only when nothing it could leak is in reach.
    if hides_state(&report).status != Status::Verified {
        let why = "stage 3 (harness execution) not run: wrapper-hides-state did not pass";
        return finish(Stages::Withheld(why), Some(&report), config_clean, None);
    }
    let command = harness.kind.command(&harness.executable, &run.home, PROMPT);
    let deadline = harness.timeout.min(STAGE_DEADLINE);
    let stage3 = match spawn::run(&layout.invocation(&command), deadline) {
        Ok(outcome) => outcome,
        Err(err) => {
            let why = format!("stage 3 (harness execution): cannot start bubblewrap: {err}");
            return finish(Stages::Failed(why), Some(&report), config_clean, None);
        }
    };
    if let Err(why) = expect_success(&stage3, deadline, Stderr::Harness) {
        let why = format!("stage 3 (harness execution): {why}");
        let starts = item(
            "harness-starts-in-wrapper",
            Status::NotProvided,
            why.clone(),
        );
        return finish(
            Stages::Failed(why),
            Some(&report),
            config_clean,
            Some(starts),
        );
    }
    let starts = match contain::extract(
        &run.root,
        home::HOME_NAME,
        harness.kind.log_pattern(),
        LOG_LIMIT,
    ) {
        Err(rejected) => item(
            "harness-starts-in-wrapper",
            Status::NotProvided,
            format!("harness log refused: {rejected}"),
        ),
        Ok(logs)
            if logs
                .iter()
                .any(|log| harness.kind.log_shows_model_response(&log.bytes)) =>
        {
            item("harness-starts-in-wrapper", Status::Verified, "")
        }
        Ok(logs) if logs.is_empty() => item(
            "harness-starts-in-wrapper",
            Status::NotProvided,
            "no session log written",
        ),
        Ok(_) => item(
            "harness-starts-in-wrapper",
            Status::NotProvided,
            "the session log shows no model response",
        ),
    };
    finish(Stages::AllRan, Some(&report), config_clean, Some(starts))
}

/// How far the three stages got.
enum Stages {
    AllRan,
    /// Later stages were not run: which ones, and why.
    Withheld(&'static str),
    Failed(String),
}

fn finish(
    stages: Stages,
    report: Option<&Report>,
    config_clean: Item,
    starts: Option<Item>,
) -> Vec<Item> {
    let wrapper_starts = match stages {
        Stages::AllRan => {
            // Optional isolation is reported as actually created (plan D5).
            let state = |name: &str| {
                let listed =
                    |list: Option<&Vec<String>>| list.is_some_and(|l| l.iter().any(|n| n == name));
                if listed(report.map(|r| &r.optional_namespaces_created)) {
                    "created"
                } else if listed(report.map(|r| &r.optional_namespaces_unavailable)) {
                    "unavailable on this kernel"
                } else {
                    "not created"
                }
            };
            let detail: Vec<String> = probe::OPTIONAL_NAMESPACES
                .iter()
                .map(|name| format!("optional {name} namespace {}", state(name)))
                .collect();
            item("wrapper-starts", Status::Verified, detail.join("; "))
        }
        Stages::Withheld(why) => item("wrapper-starts", Status::Unknown, why),
        Stages::Failed(why) => item("wrapper-starts", Status::NotProvided, why),
    };
    let (hides, reachable) = match report {
        None => (
            item(
                "wrapper-hides-state",
                Status::Unknown,
                "the probe did not run",
            ),
            item(
                "wrapper-reachable-services",
                Status::Unknown,
                "the probe did not run",
            ),
        ),
        Some(report) => {
            let hides = hides_state(report);
            let mut reached: Vec<String> = report
                .reachable_tcp
                .iter()
                .map(|a| format!("tcp {a}"))
                .collect();
            reached.extend(
                report
                    .reachable_abstract
                    .iter()
                    .map(|a| format!("unix {a}")),
            );
            let reachable = if !report.reachable_errors.is_empty() {
                let errors = report.reachable_errors.join("; ");
                item(
                    "wrapper-reachable-services",
                    Status::Unknown,
                    format!("the scan was incomplete: {errors}"),
                )
            } else if reached.is_empty() {
                item("wrapper-reachable-services", Status::Verified, "none")
            } else {
                item(
                    "wrapper-reachable-services",
                    Status::Verified,
                    reached.join(", "),
                )
            };
            (hides, reachable)
        }
    };
    let starts = starts.unwrap_or_else(|| {
        item(
            "harness-starts-in-wrapper",
            Status::Unknown,
            "the harness was not started",
        )
    });
    vec![wrapper_starts, hides, reachable, config_clean, starts]
}

/// `wrapper-hides-state` from a full-layout report: verified only when every required
/// check is present once and passed.
fn hides_state(report: &Report) -> Item {
    if let Some(why) = report.incomplete() {
        return item(
            "wrapper-hides-state",
            Status::Unknown,
            format!("the probe report is incomplete: {why}"),
        );
    }
    let failed: Vec<String> = report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    if failed.is_empty() {
        item("wrapper-hides-state", Status::Verified, "")
    } else {
        item(
            "wrapper-hides-state",
            Status::NotProvided,
            failed.join("; "),
        )
    }
}

/// The files and variables seeded for this harness.
fn seed_for(harness: &Harness) -> (Vec<SeedFile>, Vec<(String, String)>) {
    let mut files = vec![harness.kind.settings()];
    let mut env: Vec<(String, String)> = harness
        .kind
        .default_env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    env.extend(harness.env.iter().cloned());
    match &harness.credential {
        Some(Credential::Env { name, value }) => env.push((name.clone(), value.clone())),
        Some(Credential::File { bytes }) => files.push(SeedFile {
            path: harness.kind.credential_file().into(),
            bytes: bytes.clone(),
        }),
        None => {}
    }
    (files, env)
}

fn check_config(host: &Host, files: &[SeedFile], env: &[(String, String)]) -> Item {
    let mut items: Vec<(String, &[u8])> = files
        .iter()
        .map(|f| {
            (
                format!("seeded file {}", f.path.display()),
                f.bytes.as_slice(),
            )
        })
        .collect();
    items.extend(
        env.iter()
            .map(|(k, v)| (format!("seeded variable {k}"), v.as_bytes())),
    );
    let problems = home::scan_seed(&items, &host.forge_secrets);
    if problems.is_empty() {
        item("harness-config-clean", Status::Verified, "")
    } else {
        item(
            "harness-config-clean",
            Status::NotProvided,
            problems.join("; "),
        )
    }
}

/// The production layout of a run (plan D5, "Mounts").
fn run_layout(host: &Host, harness: &Harness, run: &RunDir, env: &[(String, String)]) -> Sandbox {
    let mut path: Vec<String> = Vec::new();
    if let Some(dir) = harness.executable.parent() {
        path.push(dir.display().to_string());
    }
    path.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    let mut sandbox = Sandbox::new(&host.bwrap)
        .share_net()
        .setenv("HOME", run.home.display().to_string())
        .setenv("PATH", path.join(":"))
        .setenv("LANG", host.lang.clone())
        .setenv("TERM", "dumb");
    for (key, value) in env {
        sandbox = sandbox.setenv(key.clone(), value.clone());
    }
    sandbox = sandbox.system_mounts();
    for dir in &harness.ro_binds {
        sandbox = sandbox.ro_bind(dir, dir);
    }
    sandbox
        .bind(&run.clone, &run.clone)
        .bind(&run.home, &run.home)
        .chdir(&run.clone)
}

/// Runs the probe in `sandbox`. The request goes in a file bound read-only beside the
/// probe: it lists every protected object, which can exceed what one argument holds.
/// `sandbox` must already bind `request_file` there ([`probe_sandbox`]), so that the
/// bind is among its declared mounts.
fn run_probe(sandbox: &Sandbox, request: &Request, request_file: &Path) -> Result<Report, String> {
    let json = serde_json::to_vec(request)
        .map_err(|err| format!("cannot encode the probe request: {err}"))?;
    write_private(request_file, &json)
        .map_err(|err| format!("cannot write the probe request: {err}"))?;
    let command: Vec<OsString> = vec![
        PROBE_PATH.into(),
        "__probe".into(),
        PROBE_REQUEST_PATH.into(),
    ];
    let outcome = spawn::run(&sandbox.invocation(&command), STAGE_DEADLINE)
        .map_err(|err| format!("cannot start bubblewrap: {err}"))?;
    expect_success(&outcome, STAGE_DEADLINE, Stderr::Trusted)?;
    serde_json::from_slice::<Report>(&outcome.stdout)
        .map_err(|_| "probe execution failed: no valid probe report".to_string())
        .and_then(|report| {
            if report.stage == request.stage {
                Ok(report)
            } else {
                Err("probe execution failed: report for another stage".into())
            }
        })
}

/// `sandbox` with the probe and its request file bound read-only.
fn probe_sandbox(sandbox: Sandbox, host: &Host, request_file: &Path) -> Sandbox {
    sandbox
        .ro_bind(&host.probe, PROBE_PATH)
        .ro_bind(request_file, PROBE_REQUEST_PATH)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

/// With `--unshare-pid`, bubblewrap's init is pid 1 and its child pid 2.
fn expect_pid_namespace(report: &Report) -> Result<(), String> {
    if report.pid <= 2 {
        Ok(())
    } else {
        Err(format!(
            "the probe ran as pid {} outside a new PID namespace",
            report.pid
        ))
    }
}

/// bubblewrap prefixes its own setup errors with `bwrap: ` and then exits with status 1.
/// These messages mean a namespace could not be created. The first was observed with
/// bubblewrap 0.9.0 under a user-namespace limit; the others are the messages reported
/// for user namespaces that the kernel or AppArmor refuses (Ubuntu 24.04).
const NAMESPACE_MESSAGES: [&str; 4] = [
    "Creating new namespace failed",
    "No permissions to create new namespace",
    "setting up uid map",
    "loopback: Failed",
];

/// Whose output is on the child's stderr.
#[derive(Clone, Copy)]
enum Stderr {
    /// Only bubblewrap's and the trusted probe's: bubblewrap's message is reported.
    Trusted,
    /// The harness's, which is untrusted run output: nothing of it is reported, except
    /// that bubblewrap could not start the harness at all.
    Harness,
}

fn expect_success(outcome: &Outcome, deadline: Duration, stderr: Stderr) -> Result<(), String> {
    let last_bwrap_line = String::from_utf8_lossy(&outcome.stderr)
        .lines()
        .rev()
        .find(|line| line.starts_with("bwrap: "))
        .map(str::to_string);
    let bwrap_says = match stderr {
        Stderr::Trusted => last_bwrap_line,
        Stderr::Harness => last_bwrap_line
            .filter(|line| line.starts_with("bwrap: execvp "))
            .map(|_| "bwrap: execvp failed: the harness executable could not be run".to_string()),
    };
    match outcome.ending {
        Ending::Exited(0) => Ok(()),
        Ending::TimedOut => Err(format!(
            "timed out after {} s; its process group was killed{}",
            deadline.as_secs(),
            if outcome.pipes_closed {
                " and no process of the tree survived"
            } else {
                ", but a process still held its output open"
            }
        )),
        Ending::Exited(code) => match bwrap_says {
            Some(message) if NAMESPACE_MESSAGES.iter().any(|m| message.contains(m)) => {
                Err(format!("namespace creation failed: {message}"))
            }
            Some(message) => Err(message),
            None => Err(format!("exited with status {code}")),
        },
        Ending::Signaled(signal) => Err(format!("killed by signal {signal}")),
    }
}

/// The protected objects: each path that exists, with its link count and ancestors,
/// and, for a directory, everything under it. A file under a protected directory is as
/// secret as the directory, so it is checked by its own identity and link count too.
/// Symbolic links are not followed: what they point to is not under the directory.
pub fn protected_paths(paths: &[PathBuf]) -> Result<Vec<Protected>, String> {
    use std::os::unix::fs::MetadataExt;
    let identity = |path: &std::path::Path| {
        std::fs::metadata(path)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()))
    };
    let entry = |path: PathBuf, meta: &std::fs::Metadata, ancestors| Protected {
        dev: meta.dev(),
        ino: meta.ino(),
        links: if meta.is_dir() { 0 } else { meta.nlink() },
        path,
        ancestors,
    };
    let mut protected: Vec<Protected> = Vec::new();
    // A credential file is often also under the configuration directory, so the same
    // path can be met twice: merge those, keeping every ancestor either visit found. The
    // same object under another path (a bind mount, say) stays a separate entry, since
    // each path needs its own direct-access check and keeps its own ancestors.
    let mut push = |item: Protected| {
        let same = protected
            .iter_mut()
            .find(|p| (p.dev, p.ino) == (item.dev, item.ino) && p.path == item.path);
        match same {
            Some(existing) => {
                for ancestor in item.ancestors {
                    if !existing.ancestors.contains(&ancestor) {
                        existing.ancestors.push(ancestor);
                    }
                }
            }
            None => protected.push(item),
        }
    };
    for path in paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let real = std::fs::canonicalize(path)
            .map_err(|err| format!("cannot resolve {}: {err}", path.display()))?;
        let ancestors = real.ancestors().skip(1).filter_map(identity).collect();
        push(entry(path.clone(), &meta, ancestors));
        if !meta.is_dir() {
            continue;
        }
        // Everything under it, so that a hard link to, or a bind of, any of it is seen.
        let mut stack = vec![real];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|err| {
                format!("cannot list protected directory {}: {err}", dir.display())
            })?;
            for child in entries {
                let child = child
                    .map_err(|err| {
                        format!("cannot list protected directory {}: {err}", dir.display())
                    })?
                    .path();
                let meta = std::fs::symlink_metadata(&child)
                    .map_err(|err| format!("cannot inspect {}: {err}", child.display()))?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(child.clone());
                }
                push(entry(child, &meta, Vec::new()));
            }
        }
    }
    Ok(protected)
}

/// For `kwoon __probe <request file>`: read the request, run it, and encode the report.
pub fn probe_main(request_file: &str) -> Result<String, String> {
    let json = std::fs::read(request_file)
        .map_err(|err| format!("cannot read the probe request: {err}"))?;
    let request: Request =
        serde_json::from_slice(&json).map_err(|err| format!("bad probe request: {err}"))?;
    serde_json::to_string(&probe::run(&request)).map_err(|err| err.to_string())
}
