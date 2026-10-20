# Plan: MVP evidence contract and handoff gate

- Status: draft
- Tracking issue: #4 (this plan's task: #14)
- Spec: `docs/specs/2026-10-19-mvp-evidence-and-handoff-gate.md` at `4f80544`, the head of
  PR #13. #7, #9, #11, and #13 are stacked and not merged. If the spec on `main` differs
  from `4f80544` when they merge, this plan must be checked against it before any task
  starts (`AGENTS.md`, "How work is organized").
- Drafted: 2026-10-21

## Inputs

- The spec at `4f80544`.
- Comments on #4:
  - research A, language and packaging (comment 6282728682);
  - research B, event-store engine (comment 6282691757);
  - the maintainer's decision: Rust, with Go as the fallback (comment 6292176370);
  - the update after #6: cloud sessions are out of the MVP (comment 6296641374);
  - the collected plan open questions (comment 6297457433).
- The earlier comments on cloud-session lifecycle (6281098737, 6282489660) are out of
  scope: cloud sessions are not in the MVP. They are kept for the later spec that adds
  them.

## Strategy

The long-term cost this plan reduces is the cost of changing the record format after
history exists. So the plan does three things in this order:

1. **Fail fast on the language.** The three integrations most likely to force a fallback
   to Go are proved first: a local run inside the wrapper, the two GraphQL mutations, and
   a kill test.
2. **Fix the record format, the store, and the replay rule** before any consumer is
   built on them.
3. **Keep every gate input a record.** Then shadow mode, the audit, and post-merge
   tracking are readers of the same store, not new state.

Each design choice below hides one decision behind one module (*A Philosophy of Software
Design*: deep modules, information hiding). The store hides the engine, the forge client
hides GitHub, the wrapper hides bubblewrap, and the publisher hides git. Only the gate
crate judges.

## Decisions

Each decision lists the options weighed, the choice, the reason, and where it differs
from a precedent in research A or B.

Evidence labels used below:

- **[verified 2026-10-20]**: checked while drafting this plan, against the source named.
- **[research A] / [research B]**: as stated in that comment on #4.
- **[to verify in Tn]**: assumed here; task Tn checks it before relying on it.

### D1. Event-store engine

**Options weighed** (research B, section 2):

- A. SQLite, one row per record.
- B. JSON Lines files with a hash chain.
- C. Git objects as the store.
- D. An embedded key-value store (bbolt, LMDB).
- E. SQLite holding the canonical record bytes as the source of truth (the Fossil
  pattern).
- F. A compare-and-swap "latest record" row, combined with any of the above (the Sunlight
  pattern).

**Choice: E with F.** SQLite through `rusqlite` with the `bundled` feature
(0.40.2 on crates.io [verified 2026-10-20]).

- **Records.** The table `records` has these columns:
  - `seq INTEGER PRIMARY KEY`, dense;
  - `id TEXT UNIQUE`;
  - `prev TEXT UNIQUE NOT NULL`;
  - `hash TEXT UNIQUE NOT NULL`;
  - `bytes BLOB NOT NULL`: the canonical encoding (D3).

  The blob is the record. The other columns are derived from it on insert, and
  `kwoon verify` recomputes them.
- **Head row.** A one-row table `head(seq, hash)`. Every append runs in one
  `BEGIN IMMEDIATE` transaction:
  1. Read `head`.
  2. Insert the record with `prev = head.hash` and `seq = head.seq + 1`.
  3. Update `head` with `WHERE seq = ? AND hash = ?`.
  4. Commit only if exactly one row changed.

  A second writer that read a stale head fails instead of forking the chain.
- **Derived state.** Indexes for queries ("all records for pull request n at head h")
  live in a separate file, `derived.db`. It can be deleted at any time and is rebuilt by
  one ordered scan of `records` (G5). A schema change rebuilds `derived.db`; it never
  migrates record bytes (research B, open point 12).
- **Durability.** WAL with `synchronous=FULL`. The store directory must be on a local
  filesystem. `kwoon doctor` checks this with `statfs` and refuses NFS, CIFS, and FUSE
  (research B, open point 11).
- **One writer at a time.** Every kwoon process that writes takes an exclusive `flock` on
  `store.lock` in the state directory before it opens the store for writing, and holds it
  until it exits. `flock` locks belong to the open file description, so the POSIX
  `close()` caveat in research B does not release them [to verify in T3].
- **Process shape.** There is no daemon.
  - `kwoon poll` is one pass: recover open intents, read the forge, evaluate, act,
    checkpoint, exit. A systemd user timer or cron runs it.
  - `kwoon approve`, `kwoon answer`, `kwoon policy accept`, and `kwoon init` take the
    same lock, append, and exit.
  - Read-only commands open the store read-only.

  Reading of the spec: "written by one process" means one kwoon process at a time,
  enforced by the lock and by the head row. This is listed under "Open questions" for
  confirmation.
- **Volume.** A re-evaluation writes a new `gate` record only when its input record set,
  target tip, or decision differs from the latest `gate` record for that pull request
  head. A poll that changes nothing writes nothing. T12 measures records per day by kind
  and the mean encoded size (research B, open point 1).

**Reasons.**

- One transaction covers the record insert and the head update, so a crash leaves either
  both or neither. B would need torn-line recovery, which kwoon would have to write and
  test itself.
- Queries for the audit and the gate are indexed SQL. B and C need a derived index
  anyway.
- The canonical bytes are the truth, so the engine's type conversions never enter the
  hash. The off-host copy is plain JSON Lines (D3), so the operator can read it without
  kwoon.
- No daemon means no socket for runs to reach, and recovery runs at the start of every
  pass. The crash path is therefore the normal path, not a rare one. The poll shape has
  precedents: graydon/bors, marge-bot `--cli`, and Renovate OSS (research A).
- D is out. bbolt is a Go library. LMDB allows several writers.

**Where this differs from precedents.**

- Trillian moved to Tessera, and Rekor v1 to v2, toward files plus a small coordination
  store (research B, 1.1 and 1.2). Their reasons are operating cost and read throughput
  at transparency-log scale. kwoon has one host, one reader, and low volume. At that
  scale SQLite's transaction gives the same "one sequencer, atomic pointer swap" result
  without kwoon writing its own file format.
- Litestream's format change stranded old backups (research B, 1.7). So kwoon uses no
  replication tool. The checkpoint branch (D3) is the only off-host copy, in a format
  this plan fixes.

**Second writer (G4, degraded).** A plain `sqlite3` script that skips the lock behaves
in one of two ways:

- If it inserts without updating `head`, the `UNIQUE(prev)` constraint rejects its row,
  or the next kwoon append fails on the head row. `kwoon verify` reports the fork.
- If it appends a well-formed record and updates `head`, the store accepts it. This is
  the trust model's "integrity is not authenticity" residual.

`kwoon doctor` reports G4 as degraded and cites T3's second-writer test as the proof.

### D2. Packaging

**Options weighed:**

- `cargo install` from source on the host;
- release binaries;
- an npm launcher for a native binary (Codex);
- OS packages;
- a container image.

**Choice: release binaries.**

- **Builds.** Static Linux binaries for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`. A release workflow builds them from a version tag with
  `cargo build --release --locked`. They are attached to a GitHub release with a
  `SHA256SUMS` file.
- **Install.** The operator downloads a binary and checks it with `sha256sum -c`. They
  place it anywhere outside every governed clone and run clone, for example
  `~/.local/bin`. No root is needed.
- **Update.** Only by the operator. kwoon has no self-update and no update notice that
  fetches anything.
- **Evaluator version.** Each `gate` record names it in two parts:
  - the version and commit embedded at build time;
  - the SHA-256 of the running executable, read from `/proc/self/exe` at start.
- **Released build.** Release builds carry a build flag set only by the release
  workflow. `kwoon doctor` reports the item `released-binary`. It is `verified` only for
  a release build whose path is outside every governed clone and run clone. Live mode
  needs it, because the spec says "It runs from a released version installed on the
  kwoon host". Shadow mode on a development build is allowed and is labelled as such in
  every `gate` record.
- **Toolchain.** The Rust toolchain is pinned in `rust-toolchain.toml`. `Cargo.lock` is
  committed.
- **Platforms.** Linux only in the MVP. The wrapper (D5) is bubblewrap, which is
  Linux-only. A macOS build would have no local runs, and the MVP has no other run
  source. Widening this is the maintainer's call; see "Open questions".
- **TLS.** `rustls`, so there is no OpenSSL dependency [to verify in T1: a static musl
  build makes an HTTPS request].

**Reasons.**

- `cargo install` builds kwoon from source on the host. For kwoon's own repository, that
  builds the code under judgment, which the spec forbids. It also needs a toolchain on
  the operator's machine.
- An npm launcher or a container adds a runtime the operator may not control (research
  A, C2). A container also cannot run bubblewrap or reach the operator's files without
  extra privileges.
- OS packages need root.
- musl static linking removes the dependency on the host's glibc version. `rusqlite`
  bundled compiles SQLite in, so the host's SQLite version does not matter either
  (research A).

**Where this differs from precedents.**

- Claude Code auto-updates in the background (research A, 1.2). The gate that judges
  changes must not change without the operator's action (research A, C5), so kwoon does
  not auto-update.
- Codex and Claude Code also ship macOS builds. kwoon defers macOS until it has a wrapper
  there.

### D3. Canonical encoding, hash, checkpoint format, and ruling archive

#### Canonical encoding

**Options weighed:**

- RFC 8785 (JCS) through `serde_jcs`;
- deterministic CBOR (RFC 8949, section 4.2);
- a hand-written canonical JSON for a restricted value model.

**Choice: kwoon's own RFC 8785 serializer for a restricted value model.**

- **Values.** Strings, booleans, null, arrays, objects, and integers in
  `[-(2^53-1), 2^53-1]`. Records contain no floats.
- **Object keys.** Only schema field names, which are ASCII. Data from outside, such as
  model ids, never becomes an object key; such maps are arrays of pairs. For ASCII keys,
  RFC 8785's UTF-16 key order equals byte order.
- **Encoding.** No whitespace. Strings are escaped as RFC 8785 requires: `"`, `\`, and
  control characters below U+0020, with `\b \t \n \f \r` and otherwise lowercase
  `\u00xx`; everything else is raw UTF-8. Integers are plain decimal.
- **Result.** The output is valid RFC 8785, so a second implementation (for example a
  Python JCS library) can verify the chain independently.
- **Size.** The serializer is about 150 lines in the gate crate. Its tests use the RFC
  8785 test vectors that fall inside the value model, and cross-check against
  `serde_jcs` as a test-only dependency. `serde_jcs` is never shipped.

**Reasons.**

- `serde_jcs` is at 0.2.0 [verified 2026-10-20, crates.io], as the maintainer noted. A
  defect in it would change hashes, so it must not be the production encoder.
- CBOR is not readable with `jq`, and the off-host copy should be readable without
  kwoon.
- Restricting the value model removes the hard parts of RFC 8785 (number formatting and
  UTF-16 key sorting) instead of implementing them.

**Where this differs from precedents.** Research A (O4) suggested picking a library. This
plan writes the encoder itself, because no mature Rust JCS library exists and the value
model makes the encoder small.

#### Hash and chain

- **Algorithm.** SHA-256, through `sha2` (RustCrypto, 0.11.1 [verified 2026-10-20]).
  Written as `sha256:<64 lowercase hex>`, so the algorithm is named in every hash
  (research B, open point 7: Git and Fossil both had to change hash algorithms with old
  records still present).
- **Hash input.** The bytes `kwoon-record-v1` and a newline, then the canonical encoding
  of the record without its `hash` field. `prev` is included.
- **First record.** `prev` is `genesis:<store id>`. The store id is a UUIDv7 created by
  the first `kwoon init`.
- **Record `id`.** A UUIDv7 (RFC 9562), which sorts by time, from the `uuid` crate
  (1.28.0 [verified 2026-10-20]).
- **`at`.** RFC 3339 UTC with exactly six fractional digits and `Z`.
- **Chain shape.** A linear chain, not a Merkle tree. kwoon has one verifier, the
  operator. A later spec that adds an outside verifier can add consistency proofs over
  the same records (research B, open point 9).

Options weighed for the algorithm: BLAKE3 is faster but has no tool that every operator
has. SHA-256 matches Git's SHA-256 object format, `sha256sum`, the release manifest
(D2), and C2SP checkpoints.

#### Checkpoint format

The checkpoint branch is `kwoon-checkpoints` in the policy repository. It is linear: each
commit's parent is the previous commit on the branch.

A checkpoint commit adds two things:

- `segments/<store id>/<first seq>-<last seq>.jsonl`: the canonical bytes of every record
  since the previous checkpoint, one per line, each ending in a newline. Canonical JSON
  escapes control characters, so a record never contains a raw newline.
- `CHECKPOINT`, replaced on every checkpoint:

  ```
  kwoon-checkpoint/v1
  store <store id>
  size <last seq>
  tip sha256:<hex>
  ```

The commit message is `checkpoint <store id> <size>`, with the trailers
`Kwoon-Checkpoint-Size` and `Kwoon-Checkpoint-Tip`.

- **Verification (G6).** `kwoon verify` fetches the remote branch. Every record up to the
  remote `size` must equal the local canonical bytes, and the local chain must
  recompute. A mismatch is reported with the first differing `seq`.
- **Push.** From the host's clone of the policy repository, with the git environment of
  D4. Fast-forward only, with `--force-with-lease=refs/heads/kwoon-checkpoints:<last
  pushed commit>`.
- **Recovery (G7).** Read the remote branch. If a commit with the intent's
  `Kwoon-Checkpoint-Tip` is there, record the push as done and do not push again.
- **Cadence (pilot value).** A pass pushes a checkpoint in three cases:
  - it wrote a `handoff` or an `archive` record;
  - it wrote other records and the last push is more than 10 minutes old;
  - there are unpushed records and the operator runs `kwoon checkpoint`.

  So G6's exposure window is at most 10 minutes plus one poll interval while the remote
  can be reached.
- **No signature.** A signing key on the same host adds no authenticity. That is the same
  reason the spec gives for rejecting hardware-key signatures.

Options weighed:

- tip only (C2SP `tlog-checkpoint`);
- tip plus record bodies.

Bodies were chosen. They also make the branch the off-host backup (research B, open
point 10). Without them, the host's disk would be the only full copy.

**Where this differs from precedents.** The format keeps C2SP `tlog-checkpoint`'s body
(origin, size, root) in a text file but is unsigned and holds a chain tip, not a Merkle
root (research B, 1.1). The reasons are the signature reason above and the single
verifier.

#### Ruling archive (G8; the spec says "The plan fixes the format")

- **File.** An archive commit on `kwoon-checkpoints` in the host's clone adds
  `rulings/<source record id>.json`. It is the canonical JSON of:
  - `kind: "ruling"`;
  - `source_record`: the `approval` or `answer` id;
  - `source_kind`;
  - `subject`: repository, pull request, head, and what it covers or the choice made;
  - `policy`: repository and commit in effect;
  - `reason`;
  - `at`.
- **Commit message.** `ruling <source record id>`, with the trailer
  `Kwoon-Ruling-Source: <source record id>`.
- **Verification.** Read the blob back with `git cat-file` and compare bytes. Check that
  the commit is the branch tip or an ancestor of it. Then write the `archive` record.
- **Crash recovery.** List the local branch's commits with that trailer:
  - one commit whose blob verifies: record its `archive`;
  - none: commit it;
  - anything else: `unknown`, and raise a question.

### D4. Publishing from a run's clone without running the clone's code

**Options weighed:**

1. `git push` from the run's clone. This runs the clone's hooks and reads its
   configuration: `core.sshCommand`, `credential.helper`, `url.*.insteadOf`,
   `remote.*.pushurl`, `core.fsmonitor`, `include.path`, and more. Rejected.
2. The same with `-c` overrides. This is a denylist against a configuration surface that
   grows with each git release. Rejected.
3. Fetch from the clone into a kwoon-owned repository. That runs `git upload-pack` with
   the clone as its repository, which reads the clone's configuration. Git ignores
   `uploadpack.packObjectsHook` unless it is in protected configuration [verified
   2026-10-20, git `Documentation/config/uploadpack.adoc`], but the other keys
   upload-pack reads have not been audited. Rejected.
4. Read the clone's objects in process with `gix` (0.89.0) or `git2` (0.21.0)
   [verified 2026-10-20]. `gix` is pre-1.0 and adds a large dependency. `git2` adds a C
   build. Not chosen; kept as the fallback if option 5 fails its checks in T14.
5. **Chosen:** git's own object reader and validator in a kwoon-owned repository, with
   the clone's object directory as a read-only alternate.

**Mechanism.**

1. **Publisher repository.** kwoon keeps one bare publisher repository per publication
   repository, in its state directory (hidden from runs, D5). It creates each run's clone
   from it.
2. **Git environment.** Every git command kwoon runs uses:
   - an explicit `--git-dir`, never a working directory inside a clone;
   - `GIT_CONFIG_NOSYSTEM=1`;
   - `GIT_CONFIG_GLOBAL` pointing at kwoon's own git configuration, which sets
     `core.hooksPath` to an empty directory;
   - `HOME` set to a kwoon directory;
   - `GIT_TERMINAL_PROMPT=0`.

   The forge credential is passed only through `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0`,
   and `GIT_CONFIG_VALUE_0` as an `http.extraHeader` in the child's environment. It is
   never in argv or in a file.
3. **Final head.** kwoon reads it with its own Rust reader of
   `<clone>/.git/refs/heads/kwoon/work`, or of that line in `packed-refs`.
   - The run works on that branch, which kwoon created at the start head.
   - A symbolic ref, a `reftable/` directory, or a value that is not 40 hexadecimal
     characters makes the final head `unknown`, and nothing is published.
   - No git process reads the clone's `.git` directory as a repository.
4. **Copying objects.** With `GIT_ALTERNATE_OBJECT_DIRECTORIES=<clone>/.git/objects`,
   kwoon runs `git --git-dir=<publisher> rev-list --objects <final> --not --all` piped
   into `git pack-objects --stdout`. It then imports the pack with
   `git --git-dir=<publisher> index-pack --strict --stdin`, without the alternate.
   - Git documents alternates as directories "used to search for Git objects", and
     writes no new objects to them [verified 2026-10-20, git `Documentation/git.adoc`].
   - `--strict` dies on "broken objects or links" [verified 2026-10-20,
     `git-index-pack.adoc`].

   From here on, the publisher repository holds every object, validated, and the clone is
   not touched again.
5. **Ancestry.** The spec's check that the start head is an ancestor of the final head
   runs in the publisher repository, as does this plan's extra check that the expected
   remote head (when not `absent`) is an ancestor of the final head. If either fails,
   nothing is published: the action is `failed` and a question is raised. If either
   cannot be read, the action is `unknown`.
6. **Push.**

   ```
   git --git-dir=<publisher> push --porcelain --no-verify \
     --force-with-lease=refs/heads/<branch>:<expected head, or empty for absent> \
     <url> <final>:refs/heads/<branch>
   ```

   - The refspec has no `+`.
   - The lease makes the push a compare-and-swap. Git refuses the push unless the
     remote ref equals the expected value, and an empty value means "the named ref must
     not already exist" [verified 2026-10-20, git `Documentation/git-push.adoc`].
   - Step 5 makes every update a fast-forward, so kwoon never sends a non-fast-forward
     update. The lease only adds the condition.

**Reasons.**

- No process runs with the clone as its repository.
- Objects pass git's own parser and fsck checks before they are pushed.
- The push runs entirely from a repository whose configuration kwoon wrote.
- The remaining exposure is git parsing untrusted pack data. That is the same exposure as
  any `git fetch` from an untrusted remote.

**Where this differs from precedents.** Codex and Claude Code push from inside their own
sandboxes (research A, 1.2). kwoon's publishing must hold the forge credential, so it
must not run inside the clone at all. Research A and B list no precedent for this
step.

### D5. The wrapper for local runs

**Options weighed:**

1. **Bubblewrap, denylist**: mount empty directories over known secret paths, as the
   spec gives as one way. Misses every secret path kwoon does not know about.
2. **Bubblewrap, allowlist (chosen)**: an empty home and `/tmp`, and only named paths
   bound in.
3. **Landlock** (crate `landlock` 0.4.7 [verified 2026-10-20]): works without user
   namespaces, so it could cover hosts where they are restricted. Hiding other
   processes and their sockets depends on the kernel's Landlock ABI. Codex keeps Landlock
   only as a legacy option because "it cannot isolate app-server Unix sockets"
   (research A, 1.2). Not in the MVP; see "Proposed, not in this plan".
4. **A separate OS user**: needs root; the spec rejected it.
5. **Rootless containers**: also need user namespaces, and are heavier than bubblewrap.

**Choice: bubblewrap with an allowlist.** The command line has these parts:

- `--unshare-all --share-net`: new user, PID, IPC, UTS, and cgroup namespaces. The
  network is shared, because the harness must reach its provider's API.
- `--die-with-parent --new-session --clearenv`, then `--setenv` only for `HOME`, `PATH`,
  `LANG`, `TERM`, and the variables the harness needs, from host settings.
- `--proc /proc --dev /dev --tmpfs /tmp --tmpfs <home>`.
- `--ro-bind` for `/usr` and `/etc`, and the usual `/bin`, `/lib`, and `/lib64` links.
- `--bind <run clone> <same path>`, and the run's working directory there.
- The harness's own configuration and credential paths, from host settings. For
  example, `~/.claude` and `~/.claude.json` for Claude Code, and `~/.codex` for Codex
  [to verify in T1].
- **Inner sandbox off**, through the harness's own options [to verify in T1]. For Claude
  Code `-p`, sandbox settings passed with `--settings`. For Codex `exec`,
  `--sandbox danger-full-access`.

The bubblewrap options used exist as described in its manual [verified 2026-10-20,
`containers/bubblewrap` `bwrap.xml`].

Not bound, so absent by construction:

- kwoon's state and configuration directories, including the forge token file;
- `~/.config/gh`, `~/.git-credentials`, `~/.ssh`, `~/.gnupg`;
- `/run/user/<uid>`, so the session bus and the keyring are not reachable through it;
- every other variable of the operator's environment, including `SSH_AUTH_SOCK`,
  `GH_TOKEN`, and `GITHUB_TOKEN`.

`kwoon doctor` items for the wrapper:

- **`userns-available`.** `bwrap --unshare-user true` succeeds. On Ubuntu 24.04 with
  `kernel.apparmor_restrict_unprivileged_userns=1` it can fail without root (research
  A). Then local runs are reported unavailable, as the spec requires.
- **`wrapper-hides-state`.** A probe runs inside the wrapper. It tries to read:
  - the forge token file and the store;
  - `~/.config/gh` and `~/.git-credentials`;
  - the operator's `SSH_AUTH_SOCK` path and `DBUS_SESSION_BUS_ADDRESS` (connects
    included, abstract sockets included).

  Every attempt must fail. The network namespace is shared, so abstract Unix sockets on
  the host are reachable. kwoon itself listens on no socket (D1), and the probe checks
  the operator's agent and bus addresses.
- **`harness-config-clean`.** Every path bound into the wrapper is scanned:
  - for GitHub token prefixes: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`;
  - for the exact bytes of kwoon's forge token.

  A match makes the item fail, and local runs are refused (G1).
- **`harness-starts-in-wrapper`**, per harness. One minimal prompt runs inside the
  wrapper with the inner sandbox off. The harness exits 0 and its log shows a model
  response, which proves it reached its API.

**Reasons.** An allowlist hides secrets kwoon does not know the paths of, so G1 for runs
kwoon starts does not depend on a list of known secret paths. Bubblewrap is what both
mature harnesses already use on Linux (research A).

**Where this differs from precedents.** Codex and Claude Code configure bubblewrap to
protect the host from the agent's writes. kwoon's wrapper hides the operator's
credentials from the agent's reads, so it starts from an empty home instead of the real
one.

### D6. Supporting choices

- **Forge client.**
  - Built on `ureq` 3.4.2 [verified 2026-10-20], synchronous, with `rustls` and
    `serde_json`.
  - Typed functions for exactly the REST endpoints the spec uses.
  - GraphQL as a plain POST with constant query strings and typed response structs.
  - The two mutations are `markPullRequestReadyForReview(input: {pullRequestId})` and
    `convertPullRequestToDraft(input: {pullRequestId})` [verified 2026-10-20,
    `octokit/graphql-schema` `schema.graphql`].
  - **Complete reads.** A paginated read is complete only when every `next` link was
    followed and the last page had none. Any error, rate limit, or missing page makes
    the read incomplete, and every caller turns an incomplete read into `unknown`.

  Options weighed:

  - `octocrab` (0.54.5): admits gaps in coverage and has no wrapper for the two mutations
    (the maintainer's note, research A).
  - `graphql_client` (0.16.0): compile-time checks, but needs GitHub's full schema in
    the repository, for three or four queries.

  The recorded-response tests need exact control of requests and of pagination failure,
  which a general client hides. Synchronous code is enough for one pass at a time. The
  rust-lang/bors maintainer reported slow incremental builds with async code
  (research A).
- **Forge credential.**
  - Kind: a fine-grained personal access token of the operator's account, limited to the
    governed and publication repositories.
  - Permissions: contents (write, to push branches), pull requests (write), and actions
    (write, to re-run and dispatch). Workflow-file pushes: see "Still to verify" item 8
    in T14.
  - Storage: a file in kwoon's configuration directory, mode `0600`, which the wrapper
    never binds. `kwoon doctor` checks the mode and that the file is not inside any clone.
- **Policy file.**
  - Format: TOML, parsed with `deny_unknown_fields`. Every option maps to a Rust enum or
    newtype, so the closed value sets are matched exhaustively.
  - Location: `kwoon-policy.toml` at the policy repository's root.
- **Host settings.** A TOML file in kwoon's configuration directory, not in the policy.
  It holds paths, the token file, harness commands and bound paths, each governed
  repository's publication repository and head-branch pattern, the forge API base URL,
  and the mode (shadow or live) per governed repository.
  - The forge API base URL may use plain HTTP only for loopback addresses. This exists
    for the kill-test proxy (below). `kwoon doctor` refuses live mode with a non-HTTPS
    base.
- **Workspace layout.**
  - `crates/kwoon-gate`: records, canonical encoding, policy parsing, and the evaluator.
    These judge.
  - `crates/kwoon-store`, `crates/kwoon-forge`, `crates/kwoon-run`.
  - `crates/kwoon`: the binary.

  Policy-layer paths for kwoon's own repository, when its policy is written:
  `.github/workflows/`, `crates/kwoon-gate/`, `Cargo.lock`, `rust-toolchain.toml`, and
  the policy file if it is in this repository. `Cargo.lock` is included because a
  dependency update can change what the gate decides.
- **Test harness.** Every deterministic test runs the built `kwoon` binary with a
  temporary state directory and host settings, and asserts the records written, the
  decision, and the exit code (`AGENTS.md`, "Checks and gates").
  - **Fake forge.** A test server under `tests/support/` serves REST and GraphQL from
    scenario fixtures. It keeps state (draft flag, reviewers, timeline events, workflow
    runs) and records every write.
  - **Kills without fault hooks.** The test harness gives the fake forge the pid of the
    `kwoon` process. A scenario can say "after applying write W, SIGKILL kwoon, then drop
    the connection". The forge has then accepted the request and kwoon has not recorded
    it. The production binary has no fault-injection code.
  - **Git remote kills.** A local bare repository is the publication remote. Its own
    `post-receive` hook sends the SIGKILL. That hook lives in the test's remote, never in
    a run's clone.
  - **Live kills.** A loopback proxy forwards to `api.github.com` and to git over HTTPS.
    It waits for GitHub's response, then kills kwoon before forwarding it.
  - **Fake harness.** A script stands in for the harness inside the wrapper: it writes
    commits, prints verdicts, and runs the probes of D5.
  - **Recorded responses.** T2 adds a capture script. Its responses are scrubbed of user
    names, email addresses, and tokens before commit. A CI check rejects fixtures that
    contain an email address or a token prefix (`AGENTS.md`, ground rule 1).

## Answers to the plan open questions on #4

From comment 6297457433. The two items #13 deferred (pending runs, the breaker unit) are
questions 1 and 2.

### Q1. Pending runs and incomplete reads

**Rule.**

- **Observed run set.** For each check id and commit, kwoon reads the complete list of
  runs of the policy's workflow file on that commit, every page.
- **Pending run.** A run in that list is pending when its status is not `completed` and
  it has no usable completed result for the configured job.
- **Effect on the combined result:**
  - a known failure in any run still makes the result `failed`;
  - otherwise a pending run makes it `not finished`. For a required check, gate step 4
    fails with the reason "pending: <run ids>" and raises no question; the next poll
    re-evaluates. For acceptance, the commit stays `not finished`;
  - a pending run contributes no red.
- **Incomplete read.** If the run list cannot be read completely, the result is
  `unknown`: step 4 fails and raises a question (G20), and acceptance stays
  `not finished` and raises a question.
- **Replay.**
  - Step 4's reason in the `gate` record carries the observed run set as structured data:
    run ids, status, and whether the read was complete.
  - An `acceptance` record with outcome `not finished` carries the same set.
  - Replay uses the input `check` records plus that set. A `check` record stays "one
    completed CI job attempt".

**Spec conformance.** This tightens the spec's combination rule. That rule combines only
runs represented by `check` records, so a pending run with no records does not block a
combined `passed`. Comment 6297457433 routes this question to the plan. The record-field
use is listed under "Open questions"; T6 and T17 implement the rule only after the
maintainer confirms it.

**Acceptance cases** (deterministic; T6 for required checks, T17 for acceptance).

1. On one head, a push run succeeded and a pull-request run is in progress: step 4
   fails with "pending", and no question is raised. When the run later succeeds, the
   gate passes.
2. One run failed and one is pending: the combined result is `failed`.
3. An acceptance commit has one successful run and one pending run: `not finished`, and
   no red.
4. The second page of the run list returns an error: `unknown`, the gate fails, and a
   question is raised.
5. Replay of each `gate` and `acceptance` record gives the same result.

### Q2. Breaker counting unit

**Rule.** The red count is the number of distinct `(repository, target commit)` pairs
that have at least one failed attempt of a policy acceptance job, in any run of the
acceptance workflow.

- Each pair counts once. Its time is the forge completion time of the earliest failed
  attempt on that commit.
- A re-read, another failed job, another failed run, or a later attempt on the same
  commit adds no red and does not move that time.
- A later successful attempt does not erase the historical failure.
- Missing or invalid identity or completion time makes the count `unknown` (G20).

**Reason.** The breaker exists to detect that merges without a sync turn the target red
too often. One bad commit is one such event, whatever number of triggers ran on it.
Counting runs lets one bad commit with a push run and a manual run reach the pilot
threshold (2 in 14 days) by itself.

**Spec conformance.** The spec says "distinct `(repository, workflow run id)` pairs".
This answer changes the unit, so it needs the maintainer's decision and a spec wording
change before T18 starts. See "Open questions". If the maintainer keeps runs, T18
implements the spec as written, and this answer's case 1 becomes "counts two".

**Acceptance cases** (deterministic, T18).

1. One commit has a failed push run and a failed manual run: one red, and the breaker
   stays off.
2. A second commit fails within the window: two reds, and the breaker turns on.
3. One run fails across two attempts, and is read twice: one red.
4. Replay gives the same count and breaker state.

### Q3. Gate step 8 when acceptance is `unknown` or nothing has finished yet

**Rule.** Step 8 works on the first-parent commits of the target branch, from the anchor
set with `kwoon init` up to the tip.

- **Newest finished commit.** Find the newest commit whose combined acceptance result
  is `passed` or `failed`. The target is red if that commit `failed`.
- **Newer commits.** Each commit newer than it is classified:
  - `not finished` (pending, or no run yet): skipped. It neither turns the target red
    nor makes the step `unknown`.
  - `unknown` (an unrecognized conclusion, a missing configured job in a completed
    workflow, or an incomplete read): step 8 is `unknown`. It fails with that commit as
    the reason and raises a question (G20).
- **No finished commit.** If no commit since the anchor has a finished result, step 8 is
  `unknown` and raises a question. The operator's fix is to start the acceptance
  workflow on the target (G13), or wait for it.
- **Older commits.** A commit older than the newest finished one does not affect step 8.
  This follows the spec's "newest by commit order" rule.

**Reason.** G20 forbids guessing. A pending commit is not a guess, but an `unknown` one
is.

**Spec conformance.** Within the spec: the spec defines red, and G20 governs the inputs
it cannot read.

**Acceptance cases** (deterministic, T10).

1. The newest commit is `unknown` and an older one `passed`: step 8 is `unknown`, the
   gate fails, and a question is raised.
2. The newest commit is pending and an older one `passed`: step 8 passes.
3. The newest commit is pending and an older one `failed`: red.
4. No finished result since the anchor: `unknown`, and a question.
5. An operator approval marking the pull request as the fix for the red target
   overrides case 3, and does not override cases 1 and 4.

Case 5 is this plan's reading; see "Open questions".

### Q4. Incident baseline

**Rule.** The question raised for a red target lists the merges on first-parent target
commits after the newest commit whose combined acceptance result is `passed`, up to and
including the failed commit. A run's success on a commit whose combined result is
`failed` never sets the baseline.

**Spec conformance.** The spec says "since the last passing run". This plan reads
"passing" as the combined result, the same reading the spec applies everywhere else
since #13.

**Acceptance case** (deterministic, T17). C1 is combined `passed`. C2 has one successful
run and one failed run, so C2 is combined `failed`. C3 fails. The question for C3 lists
the merges in C2 and C3, starting after C1, not after C2.

## Slices and tasks

### How the slices meet the spec's delivery order

The spec makes the first slice "one end-to-end handoff decision on kwoon's own
repository in shadow mode". Task #14 requires the first slice to prove three things
first, in a set order. Slice 1 does both:

- It starts with the three proofs: T1, T2, and T5. T3 and T4 come between T2 and T5,
  because a kill test needs a store and every record needs a policy (G18).
- It ends with the shadow decision: T6 to T12.

### Where the work stops if Rust fails

**Proof 1 (T1).** It fails in Rust if any of these does:

- a Rust process cannot start bubblewrap with the allowlist of D5;
- it cannot keep the process tree under control (die with parent, kill on timeout);
- a harness that works in bubblewrap cannot be started from Rust.

**Proof 2 (T2).** It fails in Rust if a Rust client cannot authenticate, send both
mutations, and read the result against GitHub.

**Proof 3 (T5).** It fails in Rust if the kill-and-restart test cannot run through the
built binary.

If one of them fails, the work stops at that task:

1. The implementer comments on the task issue and on #4 with the failing step, the
   evidence, and whether the failure is specific to Rust. A failure that would also
   happen in Go, such as bubblewrap blocked by the host, is not a reason to switch.
2. No later task starts.
3. A Go fallback needs a new plan. This plan is frozen once merged (`AGENTS.md`).

### Labels for acceptance rows

- **Main table rows** are named by guarantee id, with the case in brackets, for example
  "G7 [back to draft]".
- **Rows of "Publishing a run's work adds these cases"**, in table order:
  - PUB-G1a: publish with no credential;
  - PUB-G1b: hook and configuration in the clone;
  - PUB-G2: unauthorized destinations;
  - PUB-G2G20: rewritten history;
  - PUB-G7a: new branch;
  - PUB-G7b: kill between push and open;
  - PUB-G7c: someone else updates the branch;
  - PUB-G7d: the pull request is closed after opening;
  - PUB-G5: a push kwoon did not make.
- **Spec "Still to verify" items** are numbered 1 to 9 in the spec's order.

Every task runs `cargo fmt --check`, `cargo clippy --locked -- -D warnings`, and
`cargo test --locked` in CI. These are not repeated below.

---

### Slice 1: proofs, then the shadow decision

#### T1. Workspace, CI, and the local-run wrapper (proof 1)

- **Builds:**
  - the cargo workspace of D6;
  - `rust-toolchain.toml`;
  - a CI workflow for fmt, clippy, test, and the static musl build. The task issue must
    say explicitly that it may add `.github/workflows/`;
  - `crates/kwoon-run`'s wrapper (D5);
  - `kwoon doctor` printing the four wrapper items. Capability records come in T9.
- **Acceptance rows:**
  - G1 [credential and store absent inside the wrapper];
  - G4 [writing to the store fails from inside the wrapper]: the state directory is
    absent there. T3 repeats it with a real store.
- **Check first:**
  - Still to verify 7: the wrapper starts with each supported harness, and the harness
    reaches its API.
  - [plan] the options that turn off each harness's inner sandbox.
  - [plan] whether GitHub's `ubuntu-24.04` runner allows unprivileged user namespaces.
    If not, the wrapper tests run on a runner that does, and the plan's open question on
    the CI platform is answered in the PR.
  - [plan] a static musl build makes an HTTPS request with `rustls`.
- **Verification:**
  - Deterministic, `cargo test --test wrapper`. The fake harness inside the wrapper
    tries to read every absent path of D5 and to connect to the operator's agent and bus
    sockets; every attempt fails, and the probe's report is asserted.
  - Negative cases:
    - a bound harness path containing a token prefix fails `harness-config-clean`;
    - `bwrap` missing, or user namespaces denied, makes `kwoon doctor` report local runs
      unavailable with exit code 1;
    - a harness that outlives a timeout is killed with its whole process tree.
  - Live, run by the operator on the host:
    - `kwoon doctor --harness claude-code` and `--harness codex` start each real harness
      with `-p` or `exec` inside the wrapper, with the inner sandbox off;
    - the output and the probe report are posted on the task issue.
- **Depends on:** none.

#### T2. GitHub client and the two GraphQL mutations (proof 2)

- **Builds:**
  - `crates/kwoon-forge`: token file loading, the REST and GraphQL transport, and
    complete-read pagination;
  - reading a pull request (node id, `isDraft`, head, base, author);
  - `set_draft_state(pr, ready | draft)`. It reads the state first and sends the
    mutation only if the state differs, so the target state is idempotent;
  - the fake forge and the response-capture script of D6.
- **Acceptance rows:** the forge half of G7 [mark ready] and G7 [back to draft]: the
  target state is idempotent, so setting it again changes nothing. T5 and T15 add the
  kill and recovery halves.
- **Check first:** Still to verify 1, for the operator's own repository only: the
  operator's account can mark its own draft pull request ready, and convert it back.
  Which plans and repository kinds offer draft pull requests. The other two settings are
  checked in T15.
- **Verification:**
  - Deterministic, `cargo test -p kwoon-forge`, with the fake forge:
    - both mutations;
    - "already in the target state" sends no mutation;
    - a GraphQL `errors` array, HTTP 401 and 403, and a rate-limit response each give a
      typed error, never success;
    - a pagination chain with a missing page is reported incomplete.
  - Live, run by the operator: `cargo test --test live_github -- --ignored` on the
    dedicated test repository goes draft → ready → draft, and reads `isDraft` after each
    step. The captured, scrubbed responses become the fixtures.
- **Depends on:** T1.

#### T3. Event store

- **Builds:**
  - `crates/kwoon-store` (D1);
  - the canonical encoder, the record envelope, and the hash chain in
    `crates/kwoon-gate` (D3);
  - schema validation for every record kind of the spec;
  - `kwoon verify` for the local chain only.
- **Acceptance rows:**
  - G18 [a record without a policy reference is rejected];
  - G6 [an edited old record is found]: here against the local chain; T13 adds the
    pushed checkpoint;
  - G4 [store side; degraded]: the second-writer test;
  - G5: the `derived.db` rebuild mechanism. The gate-decision part is in T10.
- **Check first:** [plan] `flock` on `store.lock` survives the store's own SQLite file
  being opened and closed in the same process.
- **Verification:**
  - Deterministic tests:
    - `cargo test -p kwoon-gate canonical`: the RFC 8785 vectors inside the value model,
      and a property test against `serde_jcs`;
    - `cargo test --test store` asserts that:
      - an edited record is reported with its `seq`;
      - a record missing `policy` is rejected with a non-zero exit code;
      - a plain `sqlite3` insert that skips the head row is rejected, and one that
        updates it is accepted and reported by `kwoon verify` as valid. That is the G4
        residual, and the test asserts it as such;
      - two `kwoon` writers serialize on the lock.
  - Crash test: 300 iterations of append, `kill -9` at a random point, restart, and
    `kwoon verify`. The chain is valid, with no torn or missing record, after every
    iteration.
  - Negative: a value outside the value model (a float, a non-ASCII key) is a
    serialization error, never a silent conversion.
- **Depends on:** T1.

#### T4. Policy v1, host settings, and init

- **Builds:**
  - the policy schema for every option in "Policy v1", as closed Rust types;
  - `kwoon policy accept <commit>`, which reads the file with the git environment of D4,
    runs the floor check, and writes the `policy` record;
  - host settings loading;
  - `kwoon init <repository> --anchor <commit>`.
- **Acceptance rows:**
  - G17 [a commit that lowers a floor, or removes a policy-layer path, is refused; a
    merged policy change has no effect until accepted];
  - G8 [a policy that exists only in the store and not in git: `kwoon doctor` refuses
    to start]. T4 adds the check; T9 makes it a capability item.
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test policy`, with fixture policy repositories. The
    following are refused with a non-zero exit code, and no `policy` record is written:
    - an unknown key;
    - a value outside a closed set;
    - a lowered review count;
    - a removed policy-layer path;
    - missing acceptance checks (G13).
  - A merged but unaccepted commit leaves the old policy in effect.
- **Depends on:** T3.

#### T5. Action journal and pull-request state audit, with the kill test (proof 3)

- **Builds:**
  - `kwoon poll` as one pass under the store lock (D1);
  - `action` records with stable intent ids, and start-of-pass recovery;
  - the "mark ready" and "turn back into a draft" recovery rules of "External actions";
  - the first audit rule. A pull request opened by the operator's forge account that is
    ready for review with no `handoff` for its current head is turned back into a draft
    in live mode. In shadow mode kwoon records what it would do. In both modes it writes
    an `unaccounted` audit and a `question`;
  - `kwoon questions`.
- **Acceptance rows:**
  - G7 [back to draft: killed after the forge accepted, before recording; after restart,
    recorded once, nothing repeated];
  - G5 [a pull request marked ready without a handoff goes back to draft and a question
    is raised], deterministic. The live case is in T21.
- **Check first:** none beyond T2's.
- **Verification:**
  - Deterministic, `cargo test --test kill_back_to_draft`:
    1. The fake forge kills kwoon after applying `convertPullRequestToDraft`.
    2. The restarted `kwoon poll` reads the draft state, completes the existing intent,
       and sends no second mutation (the forge's write log has one entry).
    3. Exactly one `audit` and one `question` exist.
  - Negatives:
    - the pull request state cannot be read after the restart: the intent becomes
      `unknown`, a question is raised, and no mutation is sent;
    - shadow mode: no mutation at all.
- **Depends on:** T2, T4.
- **Go/no-go:** after T5, the implementer reports the three proofs on #4. Then T6 starts.

#### T6. CI evidence: `check` records and combined results

- **Builds:**
  - reading workflow runs and jobs for a commit, restricted to the policy's workflow file
    and job;
  - the provenance rule: the exact commit, the named workflow and job, no plain commit
    statuses;
  - job-scoped and workflow-scoped `check` records;
  - the attempt rule;
  - the combined result of "Several runs on one commit", plus Q1's pending rule and
    complete-read requirement.
- **Acceptance rows:**
  - G20 [a required check's result cannot be read];
  - G20 [two runs on one head, one failed and one passed, in either finishing order];
  - G20 [one passed and one unrecognized gives `unknown`];
  - G14 [`neutral`, `skipped`, and `timed_out` are failed; pending is not finished;
    missing or unrecognized is `unknown`]: the classification part. T17 covers the
    acceptance part;
  - Q1 cases 1, 2, and 4.
- **Check first:**
  - Still to verify 6: which commit a pull-request workflow tests (the head, or a test
    merge), and whether the forge reports it in the job or run data.
  - Still to verify 2, in part: whether upstream CI runs at all on a fork's pull request,
    and what the run list shows when it does not.
- **Verification:**
  - Deterministic, `cargo test --test checks`. Fixtures:
    - a job with the right name from another workflow: no record;
    - a commit status with the check's name: no record;
    - attempt 1 failed and attempt 2 cancelled: still failed;
    - attempt 1 failed and attempt 2 succeeded: passed;
    - two runs in both finishing orders;
    - a pending run;
    - a truncated run list.
  - Each case asserts the records, the combined result, and replay equality.
- **Depends on:** T2, T4.

#### T7. Review runs, verdicts, and findings

- **Builds:**
  - `kwoon run --role review --pr <n>`: a fresh clone at the pull request head from the
    publisher repository (D4), started in the wrapper;
  - the `run` record with launcher-stamped identity (harness, session, model and effort
    requested, provider account as an opaque id, family from the policy);
  - the observed model from the harness's log;
  - kwoon's own capture of the output;
  - a `verdict` record parsed from one final JSON block that kwoon asks for. An output
    that cannot be parsed gives `unknown`;
  - `finding` records. The prompt lists earlier findings with their ids, so the review
    can report `persists` or `fixed` against an id. kwoon validates every id it is given.
- **Acceptance rows:** G9 [a review whose model is unknown is rejected]. kwoon records
  `unknown` and never infers a model from the run's output. The gate side of G9 is in
  T10.
- **Check first:** Still to verify 3, in part: which fields the harness's log reports
  for the observed model and the session id, per harness.
- **Verification:**
  - Deterministic, `cargo test --test review_run`, with the fake harness:
    - a valid verdict;
    - an unparseable verdict gives `unknown`;
    - a verdict that names a finding id kwoon never issued is rejected;
    - a run whose output claims another model has no effect on the observed model,
      which comes from the log;
    - a log without a model field gives `unknown`.
  - Live, run by the operator: one review run per supported harness on the test
    repository.
- **Depends on:** T1, T4.

#### T8. Approvals, answers, questions, and the ruling archive

- **Builds:**
  - `kwoon approve <pr> --head <commit> --covers <what>`;
  - `kwoon answer <question> <choice>`;
  - question deadlines and lapse, derived from later records that change a fact the
    question depends on;
  - re-raising a question past its deadline;
  - the ruling archive of D3, with crash recovery;
  - "an answer acts only through new records".
- **Acceptance rows:**
  - G21 [a question's pull request gets a new head: the question lapses, and an answer
    to it has no effect; a question past its deadline is raised again];
  - G8 [an `answer` dismisses a blocking finding and an `approval` covers a
    policy-layer path; when the local archive commit fails, neither takes effect]. The
    gate's decision on this is asserted in T10;
  - G8 [killed after the archive commit, before the `archive` record: after restart,
    the existing commit is recorded and nothing is committed twice].
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test rulings`:
    - an archive commit made to fail (read-only clone): no `archive` record, and a
      question is raised;
    - a SIGKILL from a test hook in the policy clone's `post-commit`: after restart, one
      commit and one `archive`. That hook is the test's, in kwoon's clone of the test
      policy repository;
    - two commits naming the same source record: `unknown`, and a question is raised;
    - an answer to a lapsed question is refused.
- **Depends on:** T4, T5.

#### T9. Capability report

- **Builds:**
  - `capability` records for every item `kwoon doctor` tests, each with its test and
    "valid until";
  - start refusal when a floor item fails;
  - per-repository scopes;
  - `not provided` for every guarantee not built yet (spec, "Delivery order"), and for
    G22;
  - the trust-model items: whether `kwoon approve` is reachable from a process kwoon did
    not start, and whether the account has root-equivalent rights (`docker` group,
    passwordless `sudo`);
  - G4 reported as degraded, citing T3's second-writer test;
  - `released-binary` (D2).
- **Acceptance rows:**
  - G3 [a capability whose test did not run is `unknown`, never `verified`];
  - G19 [the only account of the required second family is removed, with no fallback
    written: `kwoon doctor` refuses to start];
  - G2 [the report shows `kwoon approve` from a process kwoon did not start as
    reachable, and reports root-equivalent rights];
  - G4 [the report shows G4 degraded];
  - G8 [a policy only in the store refuses start], from T4.
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test doctor`. Each item's test is forced to error, to
    time out, and to be skipped; each gives `unknown` and, for a floor item, exit code 1.
  - The family test uses host settings with one family removed.
  - The root-equivalent test runs with a fake `id` and `sudo -n` on `PATH`.
- **Depends on:** T1, T4.

#### T10. Gate evaluator (shadow) and replay

- **Builds:**
  - the nine gate steps in `crates/kwoon-gate`, writing the `gate` record (D1 dedupe
    rule);
  - `kwoon replay <gate record>`;
  - the `derived.db` rebuild check;
  - Q3's step-8 rule;
  - shadow mode: every evaluation is recorded and nothing is handed off.
- **Acceptance rows:**
  - G5 [derived state deleted and rebuilt; every gate decision is the same];
  - G2 [a forge review and comment saying "approved", and a run's output asking to be
    the fix for a red target, change nothing];
  - G9 [own-session review, unknown model, same-family review on a `cross-family` path
    are rejected; two verdicts from one run count once];
  - G10 [a new commit after an approval fails until a new review of the new head]: the
    deterministic half; the back-to-draft half is in T15;
  - G12 [shared file F and no target tip: fail; no shared file: pass without sync];
  - G15 [round limit exceeded: fail, and a question];
  - G16 [a policy-layer path with agent approvals only: fail];
  - G8 [the gate side of T8's rulings];
  - G20 [a required check unreadable: `unknown`, fail, and a question; replay gives the
    same decisions];
  - Q3 cases 1 to 5.
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test gate`. One scenario per row above, each run
    through `kwoon poll` in shadow mode against the fake forge. Each asserts the
    decision per step, the exit code, and that `kwoon replay` reproduces the record byte
    for byte.
  - The rebuild test deletes `derived.db` between two passes and compares every
    decision.
- **Depends on:** T6, T7, T8, T9.

#### T11. Release build and install

- **Builds:**
  - a release workflow, triggered by a version tag, that builds the two musl targets
    and `SHA256SUMS`. The task issue must say explicitly that it may change
    `.github/workflows/`;
  - the release build flag and the evaluator version of D2;
  - install instructions in `README.md`.
- **Acceptance rows:** G3 [a capability whose test did not run is `unknown`], for the
  `released-binary` item. It also provides the spec's "released version installed on the
  kwoon host" ("Gate and handoff"), which live mode requires (T21). No acceptance row
  tests that rule directly.
- **Check first:** none.
- **Verification:**
  - A tag on a test fork produces the binaries and `SHA256SUMS`.
  - `sha256sum -c` passes.
  - `kwoon doctor` on the downloaded binary reports `released-binary` `verified`.
  - A local `cargo build` reports it `not provided`.
  - The same binary placed inside a governed clone reports it `unknown`, with the path
    as the reason.
- **Depends on:** T9.

#### T12. Shadow operation on kwoon's own repository

- **Builds:**
  - the operator's setup of kwoon on kwoon's repository: a release from T11, an accepted
    policy, `kwoon init`, and the timer;
  - per-pull-request shadow questions that collect the operator's own decision and the
    cause of each disagreement, as `answer` records;
  - a `kwoon report shadow` command for the promotion criterion (10 consecutive matches,
    at least 3 passes);
  - the measurements of research B, open points 1 and 2: records per day by kind, mean
    encoded size, GitHub rate-limit use per pass, and rebuild time.
- **Acceptance rows:** G5 [every gate decision is the same after a rebuild], repeated on
  real records. It also meets the spec's slice-1 criterion ("Delivery order"): one
  end-to-end handoff decision on kwoon's own repository in shadow mode, with a
  replayable `gate` record.
- **Check first:** the operator decides where the policy repository lives (see "Open
  questions"). Checkpoints carry record bodies.
- **Verification:**
  - Live: at least one real pull request on kwoon's repository gets a `gate` record, and
    `kwoon replay` reproduces it.
  - The measurements are posted on #4. If records per month exceed 100,000, or a rebuild
    takes over 60 seconds, the implementer reports it before slice 2 starts.
  - Step 2 fails for agent runs started outside kwoon unless the operator covers
    authorship with an approval. That is expected until T14.
- **Depends on:** T10, T11.

---

### Slice 2: checkpoints, publishing, live handoff, and the audit

#### T13. Checkpoint push and remote verification

- **Builds:**
  - checkpoint commits and pushes (D3), as the "Push a checkpoint" external action with
    recovery;
  - `kwoon verify` against the remote branch.
- **Acceptance rows:**
  - G6 [an edited old record is found by comparison with the last pushed checkpoint];
  - G7 [push a checkpoint: killed after the remote accepted; after restart, no second
    checkpoint];
  - G8 [when the remote cannot be reached but the local archive succeeds, the gate
    passes, and the next checkpoint push carries both rulings].
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test checkpoint`. A local bare remote with a
    `post-receive` hook that kills kwoon. An unreachable remote (a bad URL) leaves the
    rulings in effect, and the next pass pushes them.
  - The edit test changes `seq` 5 in the store, recomputes the chain, and runs
    `kwoon verify`, which reports `seq` 5.
- **Depends on:** T8.

#### T14. Publishing an authoring run's work

- **Builds:**
  - `kwoon run --role author`, with the publication and governed identities fixed at
    launch from host settings;
  - the publisher of D4;
  - the "Push the run's commits" and "Open the draft pull request" actions with their
    recovery rules (hidden intent marker; search in every state);
  - `push` records, including observed pushes with author `unknown`;
  - the capability re-check before each publishing action.
- **Acceptance rows:**
  - PUB-G1a, PUB-G1b, PUB-G2, PUB-G2G20, PUB-G7a, PUB-G7b, PUB-G7c, PUB-G7d, PUB-G5;
  - G7 [push a run's commits; open a draft pull request];
  - G19 [a capability a publishing action needs lapses before that action: no external
    write].
- **Check first:**
  - Still to verify 8: whether the token needs an extra permission to push changes to
    `.github/workflows/`, and how GitHub refuses such a push, so that kwoon records it as
    `failed` and raises a question.
  - [plan] the D4 mechanism. Git invoked with `GIT_ALTERNATE_OBJECT_DIRECTORIES` reads no
    configuration from the clone, and `index-pack --strict` rejects a corrupt object
    planted in the clone. If either fails, D4 falls back to option 4 (in-process `gix`),
    and the implementer reports the change on the task issue before building it.
- **Verification:**
  - Deterministic, `cargo test --test publish`.
  - For PUB-G1b, the fake harness plants, in its clone:
    - `.git/hooks/pre-push`, `post-checkout`, and `reference-transaction`;
    - configuration keys `core.hooksPath`, `core.fsmonitor`, `core.sshCommand`,
      `credential.helper`, `url.*.insteadOf`, `url.*.pushInsteadOf`,
      `remote.origin.pushurl`, `include.path`, `gpg.program`, `filter.*`, and
      `diff.external`;
    - a `.gitattributes` filter.

    Each plant writes a canary file and tries to read the token. No canary exists after
    publishing, and only the recorded destination ref changed.
  - Kills come from the remote's `post-receive` hook and from the fake forge on
    pull-request creation.
  - Live: PUB-G1a, PUB-G7a to d, and PUB-G5 on the test repository.
- **Depends on:** T5, T7, T9.

#### T15. Live handoff and re-evaluation

- **Builds:**
  - for a passing gate: the `gate` record, the intent, mark ready, request reviewers,
    the optional handoff comment, and the `handoff` record;
  - re-evaluation of handed-off pull requests on every pass, and back to draft on
    failure, except for step 8;
  - the per-reviewer intents and the history-based recovery of "External actions";
  - the hidden-marker comment search;
  - the capability re-check before handoff;
  - live mode per repository, as a host setting.
- **Acceptance rows:**
  - G7 [mark ready; request reviewers, including every reviewer-request case in the G7
    row; post the handoff comment];
  - G10 [a handed-off pull request goes back to draft after a new commit], deterministic
    and live;
  - G19 [a capability lapses between evaluation and handoff: the handoff is refused].
- **Check first:**
  - Still to verify 5: the review-request history can be read completely, with event
    identity, and requests stay identifiable after a review or removal. This covers
    pagination, timestamp ambiguity, and unavailable history.
  - Still to verify 1, for the other two settings: who may mark ready or convert back in
    a repository the operator does not own (a fork contribution, and a company
    repository with push rights only).
- **Verification:**
  - Deterministic, `cargo test --test handoff`. Kills after each forge write.
    Reviewer-history fixtures:
    - a reviewer who reviewed before the restart;
    - a removed request;
    - a host clock and a forge clock that differ;
    - a missing history page;
    - a crash after a per-reviewer intent but before submission.
  - Live: the same on the test repository, through the kill proxy of D6.
- **Depends on:** T10, T13, T14.

#### T16. Handoff audit

- **Builds:**
  - the audit rule that a merged pull request needs a `handoff` for its head at merge;
  - the target-branch audit of first-parent commits, where the operator can push, from
    the `kwoon init` anchor;
  - the `rewritten` result;
  - the pause of new handoffs until `kwoon answer`;
  - "adopt" written as an `approval` that names the answer.
- **Acceptance rows:**
  - G11 [the merger merges at a head kwoon did not hand off; the audit flags it], live;
  - G5 [the live case of a pull request marked ready without a handoff], with T5's
    deterministic case.
- **Check first:** none.
- **Verification:**
  - Deterministic, `cargo test --test audit`:
    - a merge at an unhanded head;
    - a direct push to the target;
    - a force-push that removes the audited tip gives `rewritten`;
    - "adopt" lifts the pause, and "revert" does not.
  - Live on the test repository: merge at an unhanded head, and mark ready by hand.
- **Depends on:** T15.

---

### Slice 3: post-merge tracking

#### T17. Merge observation and acceptance tracking

- **Builds:**
  - `merge` records;
  - `acceptance` records from combined results, using the rule of T6;
  - superseding records for a late failure, and for a late `unknown` (Q1);
  - recomputing target health;
  - the red-target question with Q4's baseline;
  - time to green;
  - the incident question's choices, which include "which gate step should have caught
    this" for the spec's "Measures".
- **Acceptance rows:**
  - G14 [the target turns red: the next handoff is blocked and a question lists the
    candidate merges];
  - G14 [failed and cancelled runs kept as `check` records; rebuilt target health is
    the same];
  - G14 [two runs on one commit, one failed and one passed: `failed` in either order;
    the newest finished commit decides; a late failure on an older commit leaves a newer
    `passed` target green; a superseding `acceptance` record says `failed`];
  - G20 [acceptance: a successful configured job, plus a completed non-cancelled
    workflow with no configured-job result, gives `unknown`, `not finished`, and a
    question; superseding if the success was recorded first; no red];
  - G13 [a workflow cancelled before its job has a result: `not finished` and no red],
    deterministic part;
  - Q1 case 3, and Q4's case.
- **Check first:** none beyond T6's.
- **Verification:** deterministic, `cargo test --test acceptance`. Every finishing order
  of the multi-run fixtures, and replay equality.
- **Depends on:** T10, T16.

#### T18. Sync circuit breaker

- **Builds:**
  - the red count (Q2, after the maintainer's decision);
  - activation per repository, identified by its record;
  - the activation question;
  - the "turn off" ruling through T8;
  - refusal of stale answers;
  - step 3 reading the breaker state.
- **Acceptance rows:**
  - G12 [the breaker turns on; a "turn off" answer, archived, turns it off; an answer
    to an activation already off, or replaced, is refused; the state rebuilt after a
    restart is the same];
  - G14 [a second red in the window switches on "always sync"; one run failing across
    two attempts counts one red, and so does a re-read; a second distinct red counts two
    and turns the breaker on];
  - Q2 cases 1 to 4.
- **Check first:** the maintainer's answer to Q2 (see "Open questions").
- **Verification:** deterministic, `cargo test --test breaker`. A missing completion time
  makes the count `unknown`, and the gate fails at step 3 with a question.
- **Depends on:** T17.

#### T19. Acceptance recovery (G13)

- **Builds:**
  - per-repository capabilities for re-running the target's CI and for the manual
    trigger;
  - "Re-run a cancelled acceptance run" and "Start a missing acceptance run", with their
    recovery rules;
  - "tracked, not guaranteed" in the capability report where a capability is missing.
- **Acceptance rows:**
  - G13 [cancelled: re-run, "not accepted" meanwhile; without rights, a question;
    missing: manual start; no manual trigger: a question, and "tracked, not guaranteed"],
    live;
  - G7 [re-run and start: no second re-run, no second start; the run cannot be read
    after the crash; someone else's run on a covering commit].
- **Check first:**
  - Still to verify 2: re-run and manual-trigger rights per repository kind.
  - Still to verify 4: workflow runs on the target branch can be read completely, with
    stable run ids, triggering account, and trigger type.
- **Verification:**
  - Deterministic, `cargo test --test acceptance_recovery`, with kills after the forge
    accepted. A truncated run list makes the start intent `unknown`.
  - Live on the test repository, with a `workflow_dispatch` acceptance workflow and one
    without it.
- **Depends on:** T17.

---

### Slice 4: run settings and readiness for live

#### T20. Run settings

- **Builds:**
  - per-run model, reasoning effort, and context-compaction window through each
    harness's options, settings files in the run's clone, or environment variables in
    the wrapper;
  - recording of the requested and the observed value; a value the log does not report
    is `unknown`.
- **Acceptance rows:** G9 [a review whose model is unknown is rejected], extended to
  every per-run setting: an observed value the log does not report is `unknown` and
  satisfies no policy requirement. It also implements the spec's "Run settings"
  paragraph.
- **Check first:** Still to verify 3: the levers each harness offers per run, and which
  values its log reports.
- **Verification:**
  - Deterministic, with the fake harness: each lever reaches the harness; an unreported
    value is `unknown`.
  - Live: one run per harness and lever, with the observed values posted on the task
    issue.
- **Depends on:** T7.

#### T21. Live negative suite and readiness for live mode

- **Builds:**
  - `tests/live/`: every row of kind "Live", or "Deterministic and live", run against
    the dedicated test repository through the release binary;
  - `kwoon doctor` floor checks for live mode;
  - a report of the shadow promotion criterion.
- **Acceptance rows:** the live half of G5, G7, G10, PUB-G1a, PUB-G7a to d, and PUB-G5;
  G11; G13. Plus the spec's "Moving from shadow to live" criteria.
- **Check first:** none.
- **Verification:**
  - The operator runs `cargo test --test live -- --ignored` with the release binary and
    posts the result on #4.
  - Switching kwoon's own repository to live mode stays the operator's decision.
- **Depends on:** T12, T15, T16, T19.

## Coverage of the guarantees

| ID | Level in the MVP | Tasks |
|---|---|---|
| G1 | Degradable; provided for runs kwoon starts | T1, T14 |
| G2 | Floor | T9, T10, T14 |
| G3 | Floor | T9 |
| G4 | Degraded | T1, T3, T9 |
| G5 | Floor | T3, T5, T10, T14, T16 |
| G6 | Degraded to the last pushed checkpoint | T3, T13 |
| G7 | Floor | T5, T13, T14, T15, T19 |
| G8 | Floor | T4, T8, T9, T10, T13 |
| G9 | Degradable along the fallback chain | T7, T10, T20 |
| G10 | Floor (never carried over) | T10, T15 |
| G11 | Degraded (kwoon does not merge) | T2, T15, T16 |
| G12 | Tunable | T10, T18 |
| G13 | Floor where rights exist, degradable elsewhere | T17, T19 |
| G14 | Tunable | T6, T17, T18 |
| G15 | Tunable | T7, T10 |
| G16 | Floor (policy layer), tunable (others) | T8, T10 |
| G17 | Floor | T4 |
| G18 | Floor | T3 |
| G19 | Floor | T9, T14, T15 |
| G20 | Floor | T6, T10, T17 |
| G21 | Tunable | T5, T8 |
| G22 | Not provided in the MVP, as the spec states | T9 reports it `not provided` |

Every acceptance row of both spec tables is assigned to a task above. The live halves are
also run together in T21.

## Open questions

These are places where the spec is silent, or where this plan reads or tightens it. The
plan does not change the spec; the maintainer decides each one.

1. **Breaker unit (Q2).** The spec says `(repository, workflow run id)`. This plan
   proposes `(repository, target commit)`. T18 is blocked until the maintainer decides.
   If the answer is "commit", the spec needs a wording change first.
2. **Pending runs (Q1).** The rule tightens the spec's combination rule, and carries the
   observed run set inside the step-4 reason of `gate` and inside a `not finished`
   `acceptance`, not in a new record kind. Confirm both. If a separate record kind is
   wanted, that is a spec change.
3. **The harness log is writable by the run.** The spec takes the observed model and
   the actions from "the harness's own logs". Inside the wrapper, the agent's own tool
   calls run as the same user as the harness, so they can write that log, and the
   harness's output stream. The observed model feeds G9. This plan records it as the
   spec says. Decide whether the spec should treat the observed model as a claim, or
   whether a later task should capture it outside the run's reach.
4. **Approval for a red-target fix (Q3, case 5).** This plan reads the override in step
   8 as applying only to a red target, not to an `unknown` step 8. Confirm.
5. **Recording the mode switch.** Shadow or live mode is a host setting in this plan,
   and every `gate` record names it. The spec has no record for the operator's switch to
   live. Decide whether one is needed.
6. **"Written by one process."** This plan reads it as one kwoon process at a time,
   enforced by a lock and a compare-and-swap head (D1). Confirm.
7. **Where the policy repository lives.** Checkpoints carry every record body, including
   pull request numbers, task names, and opaque account ids. kwoon's repository is
   public. This plan recommends a separate private policy repository for the pilot. The
   operator chooses.
8. **Platforms.** Linux only in the MVP (D2). Confirm, or name the platforms to add.
9. **CI platform for wrapper tests.** If GitHub's Ubuntu runners deny unprivileged user
   namespaces, the wrapper tests need another runner. T1 reports this and proposes one.

## Proposed, not in this plan

- Landlock as a second wrapper for hosts that deny user namespaces (D5, option 3).
- `cargo deny` or a similar check on dependencies in CI.
- Signed releases.

Each would need its own issue.
