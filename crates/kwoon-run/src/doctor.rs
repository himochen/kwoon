//! The `kwoon doctor` items for the wrapper (plan D5). Every item uses the production
//! builder. An item whose test did not run is `unknown`, never `verified` (G3).

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::builder::Sandbox;
use crate::contain;
use crate::harness::Kind;
use crate::home::{self, RunDir, SeedFile};
use crate::probe::{self, PROBE_PATH, Protected, Report, Request, Stage};
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
    let stage1 = Sandbox::new(&host.bwrap)
        .share_net()
        .ro_bind(&host.probe, PROBE_PATH);
    let request = Request {
        stage: Stage::Namespace,
        protected: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        state_dir: None,
        ssh_auth_sock: None,
        dbus_address: None,
    };
    if let Err(why) = run_probe(&stage1, &request).and_then(|report| expect_pid_namespace(&report))
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
    let stage2 = layout.clone().ro_bind(&host.probe, PROBE_PATH);
    let request = Request {
        stage: Stage::Full,
        protected: host.protected.clone(),
        mounts: stage2.declared_mounts(),
        env: stage2.declared_env(),
        state_dir: Some(host.state_dir.clone()),
        ssh_auth_sock: host.ssh_auth_sock.clone(),
        dbus_address: host.dbus_address.clone(),
    };
    let report =
        match run_probe(&stage2, &request).and_then(|r| expect_pid_namespace(&r).map(|_| r)) {
            Ok(report) => report,
            Err(why) => {
                let why = format!("stage 2 (filesystem setup): {why}");
                return finish(Stages::Failed(why), None, config_clean, None);
            }
        };

    // Stage 3: the harness itself, only when nothing it could leak is in reach.
    if !report.all_ok() {
        let why = "stage 3 (harness execution) not run: wrapper-hides-state did not pass";
        return finish(Stages::Withheld(why), Some(&report), config_clean, None);
    }
    let command = harness.kind.command(&harness.executable, &run.home, PROMPT);
    let deadline = harness.timeout.min(STAGE_DEADLINE);
    let stage3 = match spawn::run(&layout.argv(&command), deadline) {
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
        Stages::AllRan => item("wrapper-starts", Status::Verified, ""),
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
            let failed: Vec<String> = report
                .checks
                .iter()
                .filter(|c| !c.ok)
                .map(|c| format!("{}: {}", c.name, c.detail))
                .collect();
            let hides = if failed.is_empty() {
                item("wrapper-hides-state", Status::Verified, "")
            } else {
                item(
                    "wrapper-hides-state",
                    Status::NotProvided,
                    failed.join("; "),
                )
            };
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
            let detail = if reached.is_empty() {
                "none".to_string()
            } else {
                reached.join(", ")
            };
            (
                hides,
                item("wrapper-reachable-services", Status::Verified, detail),
            )
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

fn run_probe(sandbox: &Sandbox, request: &Request) -> Result<Report, String> {
    let json = serde_json::to_string(request)
        .map_err(|err| format!("cannot encode the probe request: {err}"))?;
    let command: Vec<OsString> = vec![PROBE_PATH.into(), "__probe".into(), json.into()];
    let outcome = spawn::run(&sandbox.argv(&command), STAGE_DEADLINE)
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

/// The identity, link count, and ancestors of each path that exists.
pub fn protected_paths(paths: &[PathBuf]) -> Vec<Protected> {
    use std::os::unix::fs::MetadataExt;
    let identity = |path: &std::path::Path| {
        std::fs::metadata(path)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()))
    };
    paths
        .iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            let real = std::fs::canonicalize(path).ok()?;
            Some(Protected {
                dev: meta.dev(),
                ino: meta.ino(),
                path: path.clone(),
                links: if meta.is_file() { meta.nlink() } else { 0 },
                ancestors: real.ancestors().skip(1).filter_map(identity).collect(),
            })
        })
        .collect()
}

/// For `kwoon __probe`: decode the request, run it, and encode the report.
pub fn probe_main(request_json: &str) -> Result<String, String> {
    let request: Request =
        serde_json::from_str(request_json).map_err(|err| format!("bad probe request: {err}"))?;
    serde_json::to_string(&probe::run(&request)).map_err(|err| err.to_string())
}
