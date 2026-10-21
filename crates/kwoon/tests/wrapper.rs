//! `cargo test --test wrapper`: the local-run wrapper through the production entry
//! point, `kwoon doctor`, in an isolated verification context (plan T1).
//!
//! Every test runs the built `kwoon` binary with its own state directory, host
//! settings, and credentials, and asserts the items printed and the exit code.

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
provider_credential = {{ file = {provider} }}

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
            provider = quote(&self.provider),
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

#[test]
fn every_item_is_verified_with_the_correct_layout() {
    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("ok"));
    run.assert_verified();
    run.assert_no_secret();
    assert!(run.stdout.contains("kwoon doctor (test context)"));
    assert_eq!(fixture.runs_left(), 0, "the run directory is removed");
}

#[test]
fn g1_g4_credential_and_store_are_absent_inside_the_wrapper() {
    // The fake harness itself checks, as the run would: the forge credential and the
    // state directory do not exist, and writing to the store fails.
    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("look"));
    run.assert_verified();
    assert_eq!(
        fs::read_to_string(fixture.state.join("classification")).unwrap(),
        "test\n"
    );
}

#[test]
fn an_exposed_alias_of_a_protected_file_fails_the_files_check() {
    // A hard link to the forge credential, inside a tree that is otherwise allowed.
    let fixture = Fixture::new();
    fs::hard_link(&fixture.credential, fixture.bin.join("innocent-name")).unwrap();
    let run = fixture.doctor(&fixture.harness("ok"));
    let hides = run.item("wrapper-hides-state");
    let expected = format!(
        "not provided: files: {} has 2 links, and another one may be reachable",
        fixture.credential.display()
    );
    assert_eq!(hides, expected);
    // Local runs are refused: the harness is not started.
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert!(run.item("wrapper-starts").starts_with("unknown"));
    assert_eq!(run.code, 1);
    run.assert_no_secret();
}

#[test]
fn a_bound_protected_directory_fails_the_files_check() {
    // The configuration directory, which holds the credential, bound at its own path.
    let fixture = Fixture::new();
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(fixture.config.clone());
    let run = fixture.doctor(&harness);
    let hides = run.item("wrapper-hides-state");
    assert!(hides.contains("is reachable at its own path"), "{hides}");
    assert_eq!(run.code, 1);
}

/// Asserts that the files check refused the layout, so the harness never started.
fn assert_refused_before_the_harness(run: &Run, expected: &str) {
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.starts_with("not provided: files: ") && hides.contains(expected),
        "{hides}"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert!(run.item("wrapper-starts").starts_with("unknown"));
    assert_eq!(run.code, 1);
    run.assert_no_secret();
}

#[test]
fn a_hard_link_to_a_file_inside_a_protected_directory_is_refused() {
    // Review R1, finding 1: only the directory ~/.config/gh is named as protected.
    let fixture = Fixture::new();
    let hosts = fixture.home.join(".config/gh/hosts.yml");
    fs::hard_link(&hosts, fixture.bin.join("innocent-name")).unwrap();
    let run = fixture.doctor(&fixture.harness("ok"));
    let expected = format!("{} has 2 links", hosts.display());
    assert_refused_before_the_harness(&run, &expected);
}

#[test]
fn a_bound_file_inside_a_protected_directory_is_refused() {
    // Review R1, finding 1: a bind of one file under ~/.config/gh, at its own path.
    let fixture = Fixture::new();
    let hosts = fixture.home.join(".config/gh/hosts.yml");
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(hosts.clone());
    let run = fixture.doctor(&harness);
    let expected = format!("mount {0} exposes {0}", hosts.display());
    assert_refused_before_the_harness(&run, &expected);
}

#[test]
fn binding_the_operators_claude_home_is_refused() {
    // Review R1, finding 2; plan D5: the operator's ~/.claude is never bound.
    let fixture = Fixture::new();
    let claude = fixture.home.join(".claude");
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(claude.clone());
    let run = fixture.doctor(&harness);
    let expected = format!("mount {0} exposes {0}", claude.display());
    assert_refused_before_the_harness(&run, &expected);
}

#[test]
fn binding_the_operators_codex_home_is_refused() {
    let fixture = Fixture::new();
    let codex = fixture.home.join(".codex");
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(codex.clone());
    let run = fixture.doctor(&harness);
    let expected = format!("mount {0} exposes {0}", codex.display());
    assert_refused_before_the_harness(&run, &expected);
}

#[test]
fn binding_a_directory_enclosing_the_harness_homes_is_refused() {
    // Without the gh and git credentials, only the harness homes make this refusal.
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.home.join(".config")).unwrap();
    fs::remove_file(fixture.home.join(".git-credentials")).unwrap();
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(fixture.home.clone());
    let run = fixture.doctor(&harness);
    for home in [".claude", ".codex"] {
        let expected = format!(
            "mount {} exposes {}",
            fixture.home.display(),
            fixture.home.join(home).display()
        );
        assert_refused_before_the_harness(&run, &expected);
    }
}

#[test]
fn an_alias_of_the_operators_harness_credential_is_refused() {
    let fixture = Fixture::new();
    let credential = fixture.home.join(".claude/.credentials.json");
    fs::hard_link(&credential, fixture.bin.join("innocent-name")).unwrap();
    let run = fixture.doctor(&fixture.harness("ok"));
    let expected = format!("{} has 2 links", credential.display());
    assert_refused_before_the_harness(&run, &expected);
}

#[test]
fn a_writable_installation_mount_fails_even_when_declared() {
    // Review R1, finding 6: the expectation must not come from the builder alone. The
    // spy makes the harness's installation bind writable and edits the probe request
    // so that it declares the bind writable too.
    let fixture = Fixture::new();
    let log = fixture.spy(&format!("writable {}", fixture.bin.display()));
    let run = fixture.doctor(&fixture.harness("ok"));
    // The spy did change the bind, so this is the case under test.
    let log = fs::read_to_string(log).unwrap();
    assert!(
        log.contains(&format!("[--bind] [{0}] [{0}]", fixture.bin.display())),
        "{log}"
    );
    let expected = format!(
        "mounts: {} is writable, but only the run clone and the run home may be",
        fixture.bin.display()
    );
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.starts_with("not provided: ") && hides.contains(&expected),
        "{hides}"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert_eq!(run.code, 1);
}

#[test]
fn a_forbidden_variable_fails_even_when_declared() {
    // Review R1, finding 6: host settings make the builder set, and so declare, these.
    for name in ["GITHUB_TOKEN", "SSH_AUTH_SOCK"] {
        let fixture = Fixture::new();
        let mut harness = fixture.harness("ok");
        harness.env.push((name.into(), "/not-a-secret".into()));
        let run = fixture.doctor(&harness);
        let hides = run.item("wrapper-hides-state");
        assert!(
            hides.starts_with("not provided: ")
                && hides.contains(&format!("environment: forbidden variable {name}")),
            "{name}: {hides}"
        );
        assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
        assert_eq!(run.code, 1);
    }
}

#[test]
fn an_inherited_descriptor_does_not_reach_the_wrapper() {
    let fixture = Fixture::new();
    fixture.write_settings(&fixture.harness("ok"));
    let canary = fs::File::open(&fixture.canary).unwrap();
    let mut command = fixture.command();
    inherit_as_fd7(&mut command, &canary);
    let run = Run::from(command.output().unwrap());
    run.assert_verified();
    run.assert_no_secret();
}

#[test]
fn ssh_agent_and_session_bus_are_unreachable() {
    let fixture = Fixture::new();
    fixture.write_settings(&fixture.harness("ok"));
    let agent_sock = fixture.root.join("agent.sock");
    let _agent = Killed(
        Command::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&agent_sock)
            .stdout(Stdio::null())
            .spawn()
            .expect("ssh-agent is required by this test"),
    );
    wait_for("ssh-agent", Duration::from_secs(10), || agent_sock.exists());
    let bus_sock = fixture.root.join("bus");
    let _bus = UnixListener::bind(&bus_sock).unwrap();

    let mut command = fixture.command();
    command.env("SSH_AUTH_SOCK", &agent_sock).env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={},guid=0", bus_sock.display()),
    );
    Run::from(command.output().unwrap()).assert_verified();
}

#[test]
fn an_abstract_session_bus_is_reachable_and_fails_the_socket_check() {
    // Abstract sockets live in the network namespace, which runs share.
    let fixture = Fixture::new();
    fixture.write_settings(&fixture.harness("ok"));
    let name = format!("kwoon-test-bus-{}", std::process::id());
    let _bus = UnixListener::bind_addr(&UnixAddr::from_abstract_name(&name).unwrap()).unwrap();
    let mut command = fixture.command();
    command.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:abstract={name}"));
    let run = Run::from(command.output().unwrap());
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.contains(&format!("sockets: session bus reachable at @{name}")),
        "{hides}"
    );
    assert!(
        run.item("wrapper-reachable-services")
            .contains(&format!("unix @{name}"))
    );
    assert_eq!(run.code, 1);
}

#[test]
fn reachable_services_lists_loopback_ports_and_abstract_sockets() {
    let fixture = Fixture::new();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let name = format!("kwoon-test-service-{}", std::process::id());
    let unix = UnixListener::bind_addr(&UnixAddr::from_abstract_name(&name).unwrap()).unwrap();
    // This listener never accepts, and its queue is full: a blocking connect to it would
    // hang the probe until its deadline.
    // SAFETY: shrinks the accept queue of a socket we own.
    assert_eq!(unsafe { libc::listen(unix.as_raw_fd(), 0) }, 0);
    // Linux queues one connection beyond a backlog of 0; this one fills the queue.
    let _queued =
        std::os::unix::net::UnixStream::connect_addr(&UnixAddr::from_abstract_name(&name).unwrap())
            .unwrap();
    // Accept in the background so connections complete.
    thread::spawn(move || for _ in tcp.incoming() {});
    let started = Instant::now();
    let run = fixture.doctor(&fixture.harness("ok"));
    assert!(started.elapsed() < Duration::from_secs(20));
    let reachable = run.item("wrapper-reachable-services");
    assert!(
        reachable.contains(&format!("tcp 127.0.0.1:{port}")),
        "{reachable}"
    );
    assert!(reachable.contains(&format!("unix @{name}")), "{reachable}");
    // Reported, not failed: nothing named them as protected.
    run.assert_verified();
}

#[test]
fn the_operator_environment_does_not_reach_the_wrapper() {
    let fixture = Fixture::new();
    fixture.write_settings(&fixture.harness("ok"));
    let mut command = fixture.command();
    command
        .env("SSH_AUTH_SOCK", "/nonexistent/agent")
        .env("AWS_SECRET_ACCESS_KEY", CANARY);
    let run = Run::from(command.output().unwrap());
    run.assert_verified();
    run.assert_no_secret();
}

#[test]
fn run_home_changes_never_reach_the_host() {
    let fixture = Fixture::new();
    let before = fixture.host_snapshot();
    let run = fixture.doctor(&fixture.harness("mutate"));
    // The private home is writable inside: the fake harness exits 3 otherwise.
    run.assert_verified();
    assert_eq!(fixture.host_snapshot(), before);
    assert!(!Path::new("/tmp/pwned").exists());
}

fn assert_log_refused(mode: &'static str, reason: &str) {
    let fixture = Fixture::new();
    let watch = OpenWatch::new(&[&fixture.canary, &fixture.canary_dir]);
    let run = fixture.doctor(&fixture.harness(mode));
    let starts = run.item("harness-starts-in-wrapper");
    assert!(
        starts.starts_with("not provided: harness log refused: "),
        "{mode}: {starts}"
    );
    assert!(starts.ends_with(reason), "{mode}: {starts}");
    assert_eq!(run.code, 1);
    run.assert_no_secret();
    assert!(!watch.saw_any(), "{mode}: the canary was opened");
}

#[test]
fn a_symlinked_log_is_refused_without_opening_its_target() {
    assert_log_refused("log-symlink", "home/.claude/projects/*/*: symbolic link");
}

#[test]
fn a_symlinked_log_directory_is_refused_without_opening_its_target() {
    assert_log_refused("log-dir-symlink", "home/.claude/projects/*: symbolic link");
    assert_log_refused("projects-symlink", ".claude/projects: symbolic link");
}

#[test]
fn a_refused_log_name_is_not_printed() {
    // Review R1, finding 5: names under the run home are run output.
    assert_log_refused(
        "log-named-by-key",
        "home/.claude/projects/*/*: symbolic link",
    );
    assert_log_refused("dir-named-by-key", "home/.claude/projects/*: symbolic link");
}

#[test]
fn a_special_file_at_the_log_is_refused() {
    assert_log_refused(
        "log-fifo",
        "home/.claude/projects/*/*: not a regular file or directory",
    );
}

#[test]
fn a_hard_linked_log_is_refused() {
    assert_log_refused("log-hardlink", "regular file with more than one link");
}

#[test]
fn a_harness_without_a_session_log_is_not_verified() {
    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("no-log"));
    assert_eq!(
        run.item("harness-starts-in-wrapper"),
        "not provided: no session log written"
    );
    assert_eq!(run.code, 1);
}

/// A stage-2 probe report in which every check passed, for the spy to hand to kwoon in
/// place of the real one.
fn passing_report() -> serde_json::Value {
    let checks: Vec<serde_json::Value> = [
        "files",
        "store-write",
        "mounts",
        "descriptors",
        "sockets",
        "environment",
    ]
    .iter()
    .map(|name| serde_json::json!({"name": name, "ok": true, "detail": ""}))
    .collect();
    serde_json::json!({
        "stage": "full",
        "pid": 2,
        "checks": checks,
        "reachable_tcp": [],
        "reachable_abstract": [],
        "reachable_errors": [],
    })
}

/// Runs the doctor with `report` in place of the real stage-2 probe report.
fn doctor_with_report(report: &str) -> Run {
    let fixture = Fixture::new();
    let file = fixture.root.join("canned-report.json");
    fs::write(&file, report).unwrap();
    fixture.spy(&format!("report {}", file.display()));
    fixture.doctor(&fixture.harness("ok"))
}

#[test]
fn the_canned_passing_report_is_accepted() {
    // The control for the tests below: the spy's report is what kwoon reads.
    doctor_with_report(&passing_report().to_string()).assert_verified();
}

#[test]
fn a_probe_report_without_checks_is_not_verified() {
    // Review R1, finding 4: an empty check list is no evidence.
    let mut report = passing_report();
    report["checks"] = serde_json::json!([]);
    let run = doctor_with_report(&report.to_string());
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.starts_with("unknown: the probe report is incomplete: missing check files"),
        "{hides}"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert_eq!(run.code, 1);
}

#[test]
fn a_probe_report_missing_one_check_is_not_verified() {
    let mut report = passing_report();
    report["checks"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["name"] != "sockets");
    let run = doctor_with_report(&report.to_string());
    let hides = run.item("wrapper-hides-state");
    assert_eq!(
        hides,
        "unknown: the probe report is incomplete: missing check sockets"
    );
    assert_eq!(run.code, 1);
}

#[test]
fn unavailable_service_evidence_is_unknown() {
    let mut report = passing_report();
    report["reachable_errors"] = serde_json::json!(["cannot read /proc/net/tcp: denied"]);
    let run = doctor_with_report(&report.to_string());
    assert_eq!(
        run.item("wrapper-reachable-services"),
        "unknown: the scan was incomplete: cannot read /proc/net/tcp: denied"
    );
    assert_eq!(run.code, 1);
}

#[test]
fn a_malformed_probe_report_fails_stage_2() {
    for report in ["not json", ""] {
        let run = doctor_with_report(report);
        assert_eq!(
            run.item("wrapper-starts"),
            "not provided: stage 2 (filesystem setup): probe execution failed: no valid probe report",
            "{report:?}"
        );
        assert_eq!(run.code, 1);
    }
}

#[test]
fn harness_output_is_never_reported() {
    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("fail"));
    assert_eq!(
        run.item("harness-starts-in-wrapper"),
        "not provided: stage 3 (harness execution): exited with status 1"
    );
    assert!(!run.stdout.contains(&fixture.canary.display().to_string()));
    assert_eq!(run.code, 1);
}

#[test]
fn a_token_prefix_in_a_seeded_file_fails_config_clean() {
    let fixture = Fixture::new();
    fs::write(&fixture.provider, "{\"token\":\"ghp_0000\"}").unwrap();
    let run = fixture.doctor(&fixture.harness("ok"));
    assert_eq!(
        run.item("harness-config-clean"),
        "not provided: seeded file .claude/.credentials.json contains the GitHub token prefix ghp_"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert_eq!(run.code, 1);
}

#[test]
fn a_forge_credential_in_a_seeded_variable_fails_config_clean() {
    let fixture = Fixture::new();
    let mut harness = fixture.harness("ok");
    harness.env.push(("LEAK".into(), FORGE_TOKEN.into()));
    let run = fixture.doctor(&harness);
    assert_eq!(
        run.item("harness-config-clean"),
        "not provided: seeded variable LEAK contains forge credential fixture"
    );
    assert_eq!(run.code, 1);
    run.assert_no_secret();
}

#[test]
fn a_rejected_value_never_reaches_bubblewrap() {
    // Review R1, finding 3: once harness-config-clean refuses a value, no stage that
    // would carry it is built or started.
    let fixture = Fixture::new();
    let log = fixture.spy("");
    let mut harness = fixture.harness("ok");
    harness.env.push(("LEAK".into(), FORGE_TOKEN.into()));
    let run = fixture.doctor(&harness);
    assert!(run.item("harness-config-clean").starts_with("not provided"));
    assert_eq!(run.code, 1);
    let log = fs::read_to_string(log).unwrap();
    // Stage 1 ran through the spy, which carries no harness variable.
    assert!(log.contains("[__probe]"), "{log}");
    assert!(
        !log.contains(FORGE_TOKEN),
        "the rejected value reached bubblewrap:\n{log}"
    );
    assert!(!log.contains("[LEAK]"), "{log}");
    for name in ["wrapper-hides-state", "harness-starts-in-wrapper"] {
        assert!(run.item(name).starts_with("unknown"), "{name}");
    }
}

#[test]
fn each_stage_failure_is_reported_with_its_stage() {
    let fixture = Fixture::new();
    // A successful real invocation of all three stages comes first.
    fixture.doctor(&fixture.harness("ok")).assert_verified();

    // Stage 1: the user namespace is denied. kwoon runs in a user namespace whose limit
    // on nested user namespaces is 0, which needs no privilege and no particular
    // bubblewrap version.
    fixture.write_settings(&fixture.harness("ok"));
    let mut command = Command::new("unshare");
    command.args([
        "--user",
        "--map-root-user",
        "sh",
        "-c",
        "echo 0 > /proc/sys/user/max_user_namespaces && exec \"$0\" \"$@\"",
        KWOON,
    ]);
    fixture.prepare(&mut command);
    let run = Run::from(command.output().unwrap());
    let starts = run.item("wrapper-starts");
    assert!(
        starts.starts_with(
            "not provided: stage 1 (namespace setup): namespace creation failed: bwrap: "
        ),
        "{starts}\n{}",
        run.stderr
    );
    assert_eq!(run.code, 1);
    for name in ["wrapper-hides-state", "harness-starts-in-wrapper"] {
        assert!(run.item(name).starts_with("unknown"), "{name}");
    }

    // Stage 2: a bind source of the production layout is missing.
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(fixture.root.join("missing"));
    let run = fixture.doctor(&harness);
    let starts = run.item("wrapper-starts");
    assert!(
        starts
            .starts_with("not provided: stage 2 (filesystem setup): bwrap: Can't find source path"),
        "{starts}"
    );
    assert_eq!(run.code, 1);

    // Stage 3: the harness executable is missing.
    let mut harness = fixture.harness("ok");
    harness.executable = fixture.bin.join("missing-harness");
    let run = fixture.doctor(&harness);
    let starts = run.item("wrapper-starts");
    assert!(
        starts.starts_with("not provided: stage 3 (harness execution): bwrap: execvp"),
        "{starts}"
    );
    assert!(
        run.item("harness-starts-in-wrapper")
            .starts_with("not provided: stage 3")
    );
    assert_eq!(run.code, 1);
}

#[test]
fn a_harness_past_its_timeout_is_killed_with_its_process_tree() {
    let fixture = Fixture::new();
    let mut harness = fixture.harness("hang");
    harness.timeout_seconds = 2;
    let started = Instant::now();
    let run = fixture.doctor(&harness);
    assert!(started.elapsed() < Duration::from_secs(25));
    let starts = run.item("wrapper-starts");
    assert!(
        starts.contains("stage 3 (harness execution): timed out after 2 s; its process group was killed and no process of the tree survived"),
        "{starts}"
    );
    assert_eq!(run.code, 1);
    assert_eq!(processes_mentioning(&fixture.bin), Vec::<u32>::new());
}

#[test]
fn the_process_tree_dies_with_kwoon() {
    let fixture = Fixture::new();
    fixture.write_settings(&fixture.harness("hang"));
    let mut kwoon = fixture.command().stdout(Stdio::null()).spawn().unwrap();
    // Both fake-harness processes are running inside the wrapper.
    wait_for("the harness to start", Duration::from_secs(20), || {
        harness_processes(&fixture.bin).len() >= 2
    });
    kwoon.kill().unwrap();
    kwoon.wait().unwrap();
    wait_for(
        "the wrapper's processes to exit",
        Duration::from_secs(5),
        || processes_mentioning(&fixture.bin).is_empty(),
    );
}

// Controls: the probe detects each exposure that the tests above show to be absent
// inside the wrapper. It runs in a small sandbox the test builds by hand, which exposes
// exactly those things and nothing else of the host, so its scan stays inside this
// fixture.

fn failed_check(report: &serde_json::Value, name: &str) -> String {
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check {name}"));
    assert_eq!(check["ok"], false, "{name} passed: {check}");
    check["detail"].as_str().unwrap().to_string()
}

#[test]
fn the_probe_detects_each_exposure_in_a_leaky_sandbox() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.state).unwrap();
    fs::hard_link(&fixture.credential, fixture.bin.join("innocent-name")).unwrap();
    let meta = fs::metadata(&fixture.credential).unwrap();
    let request = serde_json::json!({
        "stage": "full",
        "protected": [{
            "dev": meta.dev(),
            "ino": meta.ino(),
            "path": fixture.credential,
            "links": meta.nlink(),
            "ancestors": fixture.credential.canonicalize().unwrap().ancestors().skip(1)
                .map(|dir| { let m = fs::metadata(dir).unwrap(); (m.dev(), m.ino()) })
                .collect::<Vec<_>>(),
        }],
        "mounts": [{"mount_point": "/", "access": "any", "optional": false}],
        "env": [],
        "state_dir": fixture.state,
        "ssh_auth_sock": null,
        "dbus_address": null,
        "run_clone": null,
        "run_home": null,
    });
    // The fixture root is bound at its own path below, so the probe reads this file there.
    let request_file = fixture.root.join("request.json");
    fs::write(&request_file, request.to_string()).unwrap();
    let root = fixture.root.display().to_string();
    let mut command = Command::new("bwrap");
    command
        .args([
            "--unshare-all",
            "--unshare-user",
            "--unshare-pid",
            "--die-with-parent",
        ])
        .args([
            "--clearenv",
            "--setenv",
            "EXTRA",
            "1",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
        ])
        // The whole fixture, writable, at its own path: the credential, a hard link to
        // it, and the state directory are in reach. Bound again at another path, it
        // exposes the credential under a name the probe was not told about.
        .args([
            "--bind",
            &root,
            &root,
            "--ro-bind",
            &root,
            "/exposed",
            "--ro-bind",
            KWOON,
            "/probe",
            "/probe",
            "__probe",
        ])
        .arg(&request_file)
        .env_clear()
        .stdin(Stdio::null());
    // An inherited descriptor, which bubblewrap passes on unless it is close-on-exec.
    let canary = fs::File::open(&fixture.canary).unwrap();
    inherit_as_fd7(&mut command, &canary);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let files = failed_check(&report, "files");
    assert!(files.contains("is reachable at its own path"), "{files}");
    assert!(files.contains("has 2 links"), "{files}");
    let credential = fixture.credential.display();
    assert!(
        files.contains(&format!("mount /exposed exposes {credential}")),
        "{files}"
    );
    assert!(failed_check(&report, "store-write").contains("created a file in"));
    assert!(failed_check(&report, "mounts").contains("undeclared mount /proc"));
    assert!(failed_check(&report, "descriptors").contains("extra descriptors open: 7"));
    assert!(failed_check(&report, "environment").contains("unexpected variable EXTRA"));
    assert!(!report.to_string().contains(CANARY));
}
