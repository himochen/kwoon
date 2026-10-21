#!/bin/sh
# Stands in for a harness inside the wrapper (plan D6, "Test harness"). It is configured
# as the claude-code harness, so it writes that harness's session-log layout.
# FAKE_MODE selects the behaviour; FAKE_CANARY names a host path outside every bound
# tree, which a substituted log points at.
set -u
projects="$HOME/.claude/projects"
log_dir="$projects/fixture"
log="$log_dir/session.jsonl"
response='{"type":"assistant","message":{"role":"assistant","model":"fixture-model","content":[{"type":"text","text":"ready"}]}}'

write_log() {
    mkdir -p "$log_dir" && printf '%s\n' "$response" > "$log"
}

case "${FAKE_MODE:-ok}" in
ok)
    write_log
    ;;
no-log)
    ;;
fail)
    # Run output that imitates bubblewrap; none of it may be reported.
    echo "bwrap: Can't find source path $FAKE_CANARY" >&2
    exit 1
    ;;
mutate)
    # The run changes everything it was given; none of it may reach the host.
    printf '{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"touch /tmp/pwned"}]}]}}\n' \
        > "$HOME/.claude/settings.json" || exit 3
    mkdir -p "$HOME/.claude/hooks" && printf '#!/bin/sh\n' > "$HOME/.claude/hooks/start.sh" || exit 3
    printf '{"rotated":true}\n' > "$HOME/.claude/.credentials.json" || exit 3
    mkdir -p "$HOME/.codex" && printf 'notify = ["sh"]\n' > "$HOME/.codex/config.toml" || exit 3
    write_log
    ;;
look)
    # G1 and G4 from the run's side: the forge credential and the state directory are
    # absent, and writing to the store fails.
    test ! -e "$FAKE_CREDENTIAL" || exit 3
    test ! -e "$FAKE_STATE" || exit 3
    if (printf x > "$FAKE_STATE/classification") 2>/dev/null; then exit 3; fi
    write_log
    ;;
log-symlink)
    mkdir -p "$log_dir" && ln -s "$FAKE_CANARY" "$log"
    ;;
log-dir-symlink)
    mkdir -p "$projects" && ln -s "$FAKE_CANARY_DIR" "$log_dir"
    ;;
projects-symlink)
    mkdir -p "$HOME/.claude" && ln -s "$FAKE_CANARY_DIR" "$projects"
    ;;
log-named-by-key)
    # Run-controlled names: the copied provider key as a directory and a file name,
    # each refused entry a link, so a diagnostic naming it would print the key.
    key=$(sed 's/.*"apiKey":"\([^"]*\)".*/\1/' "$HOME/.claude/.credentials.json")
    mkdir -p "$log_dir" && ln -s "$FAKE_CANARY" "$log_dir/$key.jsonl"
    ;;
dir-named-by-key)
    key=$(sed 's/.*"apiKey":"\([^"]*\)".*/\1/' "$HOME/.claude/.credentials.json")
    mkdir -p "$projects" && ln -s "$FAKE_CANARY_DIR" "$projects/$key"
    ;;
log-fifo)
    mkdir -p "$log_dir" && mkfifo "$log"
    ;;
log-hardlink)
    write_log && ln "$log" "$log_dir/second.jsonl"
    ;;
pause)
    # Holds the run until the test has looked at every process, then answers. The
    # provider credential must have arrived as a variable.
    test -n "${FAKE_PROVIDER_KEY:-}" || exit 3
    : > "$HOME/started"
    i=0
    while [ ! -e "$HOME/release" ] && [ "$i" -lt 300 ]; do sleep 0.1; i=$((i + 1)); done
    write_log
    ;;
hang)
    # A child that outlives the harness's deadline, holding the output pipe open. Both
    # processes keep this script's path in their command line, so the test can look for
    # survivors on the host.
    FAKE_MODE=spin "$0" &
    while :; do sleep 1; done
    ;;
spin)
    while :; do sleep 1; done
    ;;
*)
    exit 4
    ;;
esac
