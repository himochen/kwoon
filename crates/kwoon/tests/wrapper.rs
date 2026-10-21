//! `cargo test --test wrapper`: the local-run wrapper through the production entry
//! point, `kwoon doctor`, in an isolated verification context (plan T1).
//!
//! Every test runs the built `kwoon` binary with its own state directory, host
//! settings, and credentials, and asserts the items printed and the exit code.

include!("support/fixture.rs");

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
fn a_bind_alias_listed_first_does_not_hide_the_registered_credential() {
    // Review R2, finding 1. The forge credential lives outside the configuration
    // directory, which holds a bind-mounted alias of it: same inode, one link. The
    // directory is listed before the credential, so the alias is met first. Binding the
    // credential's own directory must still be refused.
    let mut fixture = Fixture::new();
    let secrets = fixture.root.join("secrets");
    fs::create_dir(&secrets).unwrap();
    fixture.credential = secrets.join("fixture.token");
    fs::write(&fixture.credential, format!("{FORGE_TOKEN}\n")).unwrap();
    let alias = fixture.config.join("alias");
    fs::write(&alias, "").unwrap();
    fs::remove_file(fixture.config.join("fixture.token")).unwrap();
    let mut harness = fixture.harness("ok");
    harness.ro_binds.push(secrets.clone());
    fixture.write_settings(&harness);
    // An unprivileged mount namespace, so the bind needs no root on the host.
    let mut command = Command::new("unshare");
    command.args(["--user", "--map-root-user", "--mount", "sh", "-c"]);
    command.arg(format!(
        "mount --bind {} {} && exec \"$0\" \"$@\"",
        fixture.credential.display(),
        alias.display()
    ));
    command.arg(KWOON);
    fixture.prepare(&mut command);
    let run = Run::from(command.output().unwrap());
    let expected = format!(
        "{} is reachable at its own path",
        fixture.credential.display()
    );
    assert_refused_before_the_harness(&run, &expected);
    let expected = format!(
        "mount {} exposes {}",
        secrets.display(),
        fixture.credential.display()
    );
    assert!(run.item("wrapper-hides-state").contains(&expected));
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

/// Runs the doctor with a writable bind of a host directory at `dest` added to stage 2
/// and declared writable in the probe request.
fn doctor_with_added_writable_bind(dest: &str) -> (Fixture, Run) {
    let fixture = Fixture::new();
    let source = fixture.root.join("host-dir");
    fs::create_dir(&source).unwrap();
    let log = fixture.spy(&format!("add-writable {} {dest}", source.display()));
    let run = fixture.doctor(&fixture.harness("ok"));
    let log = fs::read_to_string(log).unwrap();
    assert!(
        log.contains(&format!("[--bind] [{}] [{dest}]", source.display())),
        "{log}"
    );
    (fixture, run)
}

#[test]
fn a_writable_host_bind_under_dev_is_refused() {
    // Review R2, finding 3: /dev holds only bubblewrap's own nodes, by their mount
    // characteristics, not any mount whose destination is under /dev.
    let (_fixture, run) = doctor_with_added_writable_bind("/dev/kwoon-host");
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.starts_with("not provided: ")
            && hides.contains(
                "mounts: /dev/kwoon-host is writable, but only the run clone and the run home may be"
            ),
        "{hides}"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert_eq!(run.code, 1);
}

#[test]
fn a_host_bind_over_tmp_is_refused() {
    // /tmp must be bubblewrap's own empty tmpfs, not a host directory laid over it.
    let (_fixture, run) = doctor_with_added_writable_bind("/tmp");
    let hides = run.item("wrapper-hides-state");
    assert!(
        hides.starts_with("not provided: ") && hides.contains("mounts: /tmp is not a tmpfs"),
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
fn a_run_home_swapped_for_a_link_on_the_host_is_refused() {
    // Review R1, finding 7, through the doctor: after the harness exits, the spy (a host
    // process) replaces the run home with a link to a directory holding a valid log.
    // Following the link would read that log and verify the harness.
    let fixture = Fixture::new();
    let target = fixture.root.join("swap-target");
    let log_dir = target.join(".claude/projects/elsewhere");
    fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.jsonl");
    fs::write(
        &log,
        "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}}\n",
    )
    .unwrap();
    fixture.spy(&format!("swap-home {}", target.display()));
    let watch = OpenWatch::new(&[&target, &log_dir, &log]);
    let run = fixture.doctor(&fixture.harness("ok"));
    assert_eq!(
        run.item("harness-starts-in-wrapper"),
        "not provided: harness log refused: home: symbolic link"
    );
    assert_eq!(run.code, 1);
    assert!(!watch.saw_any(), "the link target was opened");
    // Removing the run directory removed the link, not what it points to.
    assert!(log.exists());
    assert_eq!(fixture.runs_left(), 0);
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
        "namespaces",
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
        "optional_namespaces_created": ["cgroup"],
        "optional_namespaces_unavailable": [],
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
fn the_optional_cgroup_namespace_is_reported_as_created() {
    // Review R1, finding 8: plan D5 reports optional isolation as actually created.
    let fixture = Fixture::new();
    let run = fixture.doctor(&fixture.harness("ok"));
    run.assert_verified();
    assert_eq!(
        run.item("wrapper-starts"),
        "verified: optional cgroup namespace created"
    );
}

#[test]
fn an_optional_cgroup_namespace_not_created_is_reported() {
    // The spy asks bubblewrap for the required namespaces only.
    let fixture = Fixture::new();
    fixture.spy("no-cgroup");
    let run = fixture.doctor(&fixture.harness("ok"));
    run.assert_verified();
    assert_eq!(
        run.item("wrapper-starts"),
        "verified: optional cgroup namespace not created"
    );
}

#[test]
fn an_optional_namespace_the_kernel_lacks_is_reported_unavailable() {
    // Review R2, finding 4: a kernel without cgroup namespaces is not an error.
    let mut report = passing_report();
    report["optional_namespaces_created"] = serde_json::json!([]);
    report["optional_namespaces_unavailable"] = serde_json::json!(["cgroup"]);
    let run = doctor_with_report(&report.to_string());
    run.assert_verified();
    assert_eq!(
        run.item("wrapper-starts"),
        "verified: optional cgroup namespace unavailable on this kernel"
    );
}

#[test]
fn the_canned_passing_report_is_accepted() {
    // The control for the tests below: the spy's report is what kwoon reads.
    doctor_with_report(&passing_report().to_string()).assert_verified();
}

#[test]
fn the_spy_substitutes_no_report_outside_a_test_context() {
    // Review R2, finding 2: a normal context naming the spy as its bubblewrap gets no
    // canned report, so the harness never starts on evidence the probe did not give.
    let fixture = Fixture::new();
    let file = fixture.root.join("canned-report.json");
    fs::write(&file, passing_report().to_string()).unwrap();
    fixture.spy(&format!("report {}", file.display()));
    fixture.write_settings(&fixture.harness("ok"));
    let settings = fixture.config.join("settings.toml");
    let text = fs::read_to_string(&settings).unwrap();
    let test_context = "kind = \"test\"\nfixture_repositories = [\"kwoon-fixtures/sandbox\"]\nfixture_endpoints = [\"https://api.github.com\"]";
    assert!(text.contains(test_context));
    fs::write(&settings, text.replace(test_context, "kind = \"normal\"")).unwrap();
    let run = Run::from(fixture.command().output().unwrap());
    assert!(
        run.stdout.contains("kwoon doctor (normal context)"),
        "{}",
        run.stdout
    );
    let starts = run.item("wrapper-starts");
    assert!(
        starts.starts_with("not provided: stage 2 (filesystem setup)"),
        "{starts}"
    );
    assert!(run.item("harness-starts-in-wrapper").starts_with("unknown"));
    assert_eq!(run.code, 1);
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
        "host_namespaces": [],
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
