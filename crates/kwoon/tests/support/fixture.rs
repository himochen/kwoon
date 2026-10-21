// The fixture shared by the integration tests, included with `include!` so that each
// test binary gets its own copy without a module of public items.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr as UnixAddr, UnixListener};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const KWOON: &str = env!("CARGO_BIN_EXE_kwoon");
const FAKE_HARNESS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fake-harness.sh");
const BWRAP_SPY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/bwrap-spy.sh");

/// Bytes that must never appear in anything kwoon prints.
const CANARY: &str = "canary-bytes-6f1d2c";
const FORGE_TOKEN: &str = "fixture-forge-token-9b7e";
const PROVIDER_CREDENTIAL: &str = "{\"apiKey\":\"fixture-provider-key\"}\n";

const ITEMS: [&str; 5] = [
    "wrapper-starts",
    "wrapper-hides-state",
    "wrapper-reachable-services",
    "harness-config-clean",
    "harness-starts-in-wrapper",
];

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    bin: PathBuf,
    /// The operator's home on the host.
    home: PathBuf,
    state: PathBuf,
    runs: PathBuf,
    credential: PathBuf,
    provider: PathBuf,
    canary: PathBuf,
    canary_dir: PathBuf,
}

/// The harness part of host settings, for one test.
struct HarnessSettings {
    mode: &'static str,
    executable: PathBuf,
    ro_binds: Vec<PathBuf>,
    timeout_seconds: u64,
    env: Vec<(String, String)>,
    /// The provider credential from this variable of kwoon's environment, instead of
    /// the default credential file.
    provider_env: Option<&'static str>,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn from(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// The status and detail printed for `name`.
    fn item(&self, name: &str) -> String {
        let prefix = format!("  {name}: ");
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no {name} in output:\n{}\n{}", self.stdout, self.stderr))
            .to_string()
    }

    fn assert_verified(&self) {
        for name in ITEMS {
            assert!(
                self.item(name).starts_with("verified"),
                "{name} is not verified:\n{}\n{}",
                self.stdout,
                self.stderr
            );
        }
        assert_eq!(self.code, 0, "{}\n{}", self.stdout, self.stderr);
    }

    fn assert_no_secret(&self) {
        for secret in [CANARY, FORGE_TOKEN, "fixture-provider-key"] {
            assert!(
                !self.stdout.contains(secret) && !self.stderr.contains(secret),
                "{secret} leaked"
            );
        }
    }
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let fixture = Fixture {
            config: root.join("config"),
            bin: root.join("bin"),
            home: root.join("operator-home"),
            state: root.join("state"),
            runs: root.join("runs"),
            credential: root.join("config/fixture.token"),
            provider: root.join("provider-credential.json"),
            canary: root.join("canary/secret"),
            canary_dir: root.join("canary"),
            root,
            _dir: dir,
        };
        for dir in [&fixture.config, &fixture.bin, &fixture.canary_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        let harness = fixture.bin.join("fake-harness");
        fs::copy(FAKE_HARNESS, &harness).unwrap();
        fs::set_permissions(&harness, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&fixture.credential, format!("{FORGE_TOKEN}\n")).unwrap();
        fs::write(&fixture.provider, PROVIDER_CREDENTIAL).unwrap();
        fs::write(&fixture.canary, CANARY).unwrap();
        // The operator's own harness homes and git credentials, which no run may change
        // or read.
        for (path, bytes) in [
            (".claude/settings.json", "{\"model\":\"operator\"}\n"),
            (".claude/.credentials.json", "{\"operator\":true}\n"),
            (".codex/config.toml", "model = \"operator\"\n"),
            (".codex/auth.json", "{\"operator\":true}\n"),
            (
                ".config/gh/hosts.yml",
                "github.com:\n  oauth_token: operator\n",
            ),
            (".git-credentials", "https://operator:operator@github.com\n"),
        ] {
            let path = fixture.home.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fixture
    }

    fn harness(&self, mode: &'static str) -> HarnessSettings {
        HarnessSettings {
            mode,
            executable: self.bin.join("fake-harness"),
            ro_binds: vec![self.bin.clone()],
            timeout_seconds: 20,
            env: Vec::new(),
            provider_env: None,
        }
    }

    fn write_settings(&self, harness: &HarnessSettings) {
        let quote = |path: &Path| format!("{:?}", path.display().to_string());
        let binds: Vec<String> = harness.ro_binds.iter().map(|p| quote(p)).collect();
        let mut env = vec![
            ("FAKE_MODE".to_string(), harness.mode.to_string()),
            ("FAKE_CANARY".into(), self.canary.display().to_string()),
            (
                "FAKE_CANARY_DIR".into(),
                self.canary_dir.display().to_string(),
            ),
            (
                "FAKE_CREDENTIAL".into(),
                self.credential.display().to_string(),
            ),
            ("FAKE_STATE".into(), self.state.display().to_string()),
        ];
        env.extend(harness.env.iter().cloned());
        let env: Vec<String> = env.iter().map(|(k, v)| format!("{k} = {v:?}")).collect();
        let text = format!(
            r#"[context]
kind = "test"
fixture_repositories = ["kwoon-fixtures/sandbox"]
fixture_endpoints = ["https://api.github.com"]

[paths]
state_dir = {state}
runs_dir = {runs}
bwrap = {bwrap}

[credentials.fixture]
file = {credential}

[harness.claude-code]
executable = {executable}
ro_binds = [{binds}]
timeout_seconds = {timeout}
provider_credential = {provider}

[harness.claude-code.env]
{env}
"#,
            state = quote(&self.state),
            runs = quote(&self.runs),
            bwrap = quote(&self.bwrap()),
            credential = quote(&self.credential),
            executable = quote(&harness.executable),
            binds = binds.join(", "),
            timeout = harness.timeout_seconds,
            provider = match harness.provider_env {
                Some(name) => format!("{{ env = {name:?} }}"),
                None => format!("{{ file = {} }}", quote(&self.provider)),
            },
            env = env.join("\n"),
        );
        fs::write(self.config.join("settings.toml"), text).unwrap();
    }

    /// The bubblewrap kwoon is told to use: the spy once [`Fixture::spy`] installed it.
    fn bwrap(&self) -> PathBuf {
        let spy = self.root.join("spy/bwrap");
        if spy.exists() {
            spy
        } else {
            "/usr/bin/bwrap".into()
        }
    }

    /// Puts the bubblewrap spy in place, outside every tree bound into the wrapper, with
    /// `mode` (see `tests/support/bwrap-spy.sh`). Returns its log.
    fn spy(&self, mode: &str) -> PathBuf {
        let dir = self.root.join("spy");
        fs::create_dir_all(&dir).unwrap();
        let spy = dir.join("bwrap");
        fs::copy(BWRAP_SPY, &spy).unwrap();
        fs::set_permissions(&spy, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("mode"), format!("{mode}\n")).unwrap();
        dir.join("log")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(KWOON);
        self.prepare(&mut command);
        command
    }

    /// The operator's environment: their home, and the variables a run must not see.
    fn prepare(&self, command: &mut Command) {
        command
            .args([
                "--config-dir".as_ref(),
                self.config.as_os_str(),
                "doctor".as_ref(),
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("GH_TOKEN", FORGE_TOKEN)
            .env("GITHUB_TOKEN", FORGE_TOKEN)
            .stdin(Stdio::null());
    }

    fn doctor(&self, harness: &HarnessSettings) -> Run {
        self.write_settings(harness);
        Run::from(self.command().output().unwrap())
    }

    /// Files under the operator's home and the provider credential source, with their
    /// bytes.
    fn host_snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = vec![(self.provider.clone(), fs::read(&self.provider).unwrap())];
        let mut stack = vec![self.home.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
        }
        files.sort();
        files
    }

    fn runs_left(&self) -> usize {
        fs::read_dir(&self.runs).map(|d| d.count()).unwrap_or(0)
    }
}

/// Watches a path for any open or read, to prove a canary was never opened.
struct OpenWatch {
    fd: std::os::fd::OwnedFd,
}

impl OpenWatch {
    fn new(paths: &[&Path]) -> OpenWatch {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: plain syscalls; the descriptor is owned below.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(raw >= 0);
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        for path in paths {
            let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let mask = libc::IN_OPEN | libc::IN_ACCESS;
            assert!(unsafe { libc::inotify_add_watch(raw, c.as_ptr(), mask) } >= 0);
        }
        OpenWatch { fd }
    }

    fn saw_any(&self) -> bool {
        let mut buf = [0u8; 4096];
        // SAFETY: reading into a local buffer from our inotify descriptor.
        let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        n > 0
    }
}

/// Host processes of the fake harness itself (its shell), leaving out bubblewrap,
/// whose own command line names the harness too.
fn harness_processes(bin: &Path) -> Vec<u32> {
    let script = format!("{}\0", bin.join("fake-harness").display());
    processes_mentioning(bin)
        .into_iter()
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                let c = String::from_utf8_lossy(&c);
                !c.split('\0').next().unwrap_or("").ends_with("bwrap") && c.contains(&script)
            })
        })
        .collect()
}

/// Host processes whose command line mentions `needle`.
fn processes_mentioning(needle: &Path) -> Vec<u32> {
    let needle = needle.display().to_string();
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if let Ok(cmdline) = fs::read(entry.path().join("cmdline"))
            && String::from_utf8_lossy(&cmdline).contains(&needle)
        {
            found.push(pid);
        }
    }
    found
}

fn wait_for(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Makes `file` descriptor 7 of the child, open across exec.
fn inherit_as_fd7(command: &mut Command, file: &fs::File) {
    let raw = file.as_raw_fd();
    // SAFETY: async-signal-safe calls in the child between fork and exec. dup2 onto the
    // same number keeps close-on-exec, so the flag is cleared explicitly.
    unsafe {
        command.pre_exec(move || {
            if (raw != 7 && libc::dup2(raw, 7) < 0) || libc::fcntl(7, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
