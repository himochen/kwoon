# Plan: MVP evidence contract and handoff gate

- Status: draft
- Tracking issue: #4 (this plan's task: #14)
- Spec: `docs/specs/2026-10-19-mvp-evidence-and-handoff-gate.md` at `4f80544`, the head of
  PR #13. #7, #9, #11, and #13 are stacked and not merged. If the spec on `main` differs
  from `4f80544` when they merge, this plan must be checked against it before any task
  starts (`AGENTS.md`, "How work is organized").
- Drafted: 2026-10-21
- Revised after the review of `24a470c` (PR #15, comment 6298150357), and after the
  round-2 review of `b673730` (comments 6298701949 and 6298711520), and after the
  round-3 review of `cd4c8ca` (comment 6299052089).

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
- The review of this plan at `24a470c` (PR #15, comment 6298150357), and its decisions on
  the open questions.

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
Design*: deep modules, information hiding):

- the store hides the engine;
- the forge client hides GitHub, including which credential each call uses;
- the wrapper hides bubblewrap;
- the publisher hides git.

Only the gate crate judges.

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
- **Consistency check before every write.** Before its first append, every kwoon write
  command checks four things:
  - `head` names the record with the highest `seq`;
  - that record's `hash` recomputes from its bytes;
  - `seq` has no gap;
  - each record's `prev` is the `hash` of the record before it.

  The first three checks cover the whole table. The `prev` check covers the records
  added since the last full `kwoon verify`. If any check fails, the command writes
  nothing, makes no external call, and exits non-zero with the first inconsistent `seq`.
  `kwoon doctor` reports `store-consistent` as failed. Only the operator can resolve this,
  by restoring from the checkpoint branch; that procedure is part of T13.
- **Derived state.** Indexes for queries ("all records for pull request n at head h")
  live in a separate file, `derived.db`. It can be deleted at any time and is rebuilt by
  one ordered scan of `records` (G5). A schema change rebuilds `derived.db`; it never
  migrates record bytes (research B, open point 12).
- **Durability.** WAL with `synchronous=FULL`. The store directory must be on a local
  filesystem. `kwoon doctor` checks this with `statfs` and refuses NFS, CIFS, and FUSE
  (research B, open point 11).
- **One authoritative writer at a time** (decided on #15, OQ6). Every kwoon process that
  writes takes an exclusive `flock` on `store.lock` in the state directory. The lock
  covers only the steps listed under "Lock scope". `flock` locks belong to the open file
  description, so the POSIX `close()` caveat in research B does not release them [to
  verify in T3].
- **Lock scope.** The lock is held for bounded steps only:
  - **`kwoon poll`**: one pass, which is a sequence of forge reads, evaluations, and
    external calls, each with a deadline (D6). The pass also has an overall deadline of 5
    minutes; when it expires, the pass stops starting new external actions, records what
    it has confirmed, and exits non-zero.
  - **`kwoon run`**: three short holds.
    1. Write the `run` record that authorizes the launch, then release the lock.
    2. The agent works without the lock.
    3. Take the lock again to write the run's outcome, the publish intents, and the
       publishing actions, each with its deadline.
  - **`kwoon approve`, `kwoon answer`, `kwoon policy accept`, `kwoon init`**: append,
    then release.
  - **Waiting.** A command waits at most 120 seconds for the lock. Then it exits
    non-zero with "store busy" and writes nothing.

  Read-only commands open the store read-only and take no lock.
- **Process shape.** There is no daemon. A systemd user timer or cron runs `kwoon poll`.
  A pass starts by recovering open intents, so the crash path runs on every pass.
- **Volume.** A re-evaluation writes a new `gate` record only when its input record set,
  observed run sets (Q1), target tip, or decision differs from the latest `gate` record
  for that pull request head. A poll that changes nothing writes nothing. T12 measures
  records per day by kind and the mean encoded size (research B, open point 1).

**Reasons.**

- One transaction covers the record insert and the head update, so a crash leaves either
  both or neither. B would need torn-line recovery, which kwoon would have to write and
  test itself.
- Queries for the audit and the gate are indexed SQL. B and C need a derived index
  anyway.
- The canonical bytes are the truth, so the engine's type conversions never enter the
  hash. The off-host copy is plain JSON Lines (D3), so the operator can read it without
  kwoon.
- No daemon means no socket for runs to reach. Bounded lock holds mean that a long agent
  run blocks neither a poll nor an operator's answer.
- D is out:
  - bbolt is a Go library;
  - LMDB serializes write transactions, but any process may open the environment for
    writing, so it gives no exclusion; kwoon would still need the lock and the head row.

**Where this differs from precedents.**

- Trillian moved to Tessera, and Rekor v1 to v2, toward files plus a small coordination
  store (research B, 1.1 and 1.2). Their reasons are operating cost and read throughput
  at transparency-log scale. kwoon has one host, one reader, and low volume. At that
  scale SQLite's transaction gives the same "one sequencer, atomic pointer swap" result
  without kwoon writing its own file format.
- Litestream's format change stranded old backups (research B, 1.7). So kwoon uses no
  replication tool. The checkpoint branch (D3) is the only off-host copy, in a format
  this plan fixes.

**A raw writer (G4, degraded).** A process that writes the SQLite file without kwoon has
two outcomes:

- A raw insert may succeed while leaving `head` inconsistent. Verification and the next
  kwoon write must detect that inconsistency and refuse further authoritative work.
- A well-formed append that also updates `head` remains the documented authenticity
  residual.

`kwoon doctor` reports G4 as degraded and cites T3's raw-writer test as the proof.

### D2. Packaging

**Options weighed:**

- `cargo install`, from a registry version or from source;
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
- **Update.** Only by the operator. kwoon has no self-update and no update notice.
- **Evaluator version.** Each `gate` record names it in two parts:
  - the version and commit embedded at build time;
  - the SHA-256 of the running executable, read from `/proc/self/exe` at start.
- **Released binary.** `kwoon doctor` reports the item `released-binary`. It is
  `verified` when both of these hold:
  - the executable's SHA-256 appears in the `SHA256SUMS` of kwoon's GitHub release for
    the embedded version. kwoon reads that file as a public, read-only download;
  - the executable's path is outside every governed clone and run clone.

  Otherwise the item is `not provided`, or `unknown` when the release cannot be read.
  The spec says the gate "runs from a released version installed on the kwoon host".
  Every normal governed repository requires a released binary in both shadow and live
  modes. Development execution is confined to the approved isolated verification context
  in D6.
- **Toolchain.** The Rust toolchain is pinned in `rust-toolchain.toml`. `Cargo.lock` is
  committed.
- **Platforms.** Linux only in the MVP (decided on #15, OQ8). The wrapper (D5) is
  bubblewrap, which is Linux-only.
- **TLS.** `rustls`, so there is no OpenSSL dependency [to verify in T1: a static musl
  build makes an HTTPS request].

**Reasons.**

- `cargo install kwoon --version <v>` would build a published registry version rather
  than the working tree, so it avoids building the code under judgment. It still
  compiles on the operator's machine, which needs a toolchain and runs the build scripts
  of every dependency there. A release binary moves both to the release workflow, and its
  checksum can be checked against the release.
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
- Codex and Claude Code also ship macOS builds. kwoon has no wrapper there in the MVP.

### D3. Canonical encoding, hash, checkpoint format, and ruling archive

#### Canonical encoding

**Options weighed:**

- `serde_jcs` (0.2.0 [verified 2026-10-20]): RFC 8785;
- `serde_json_canonicalizer` (0.4.1, 12.8 million downloads [verified 2026-10-20]): RFC
  8785, including number formatting;
- deterministic CBOR (RFC 8949, section 4.2);
- a hand-written RFC 8785 serializer for a restricted value model.

**Choice: kwoon's own RFC 8785 serializer for a restricted value model.**

- **Values.** Strings, booleans, null, arrays, objects, and integers in
  `[-(2^53-1), 2^53-1]`. Records contain no floats.
- **Object keys.** Only schema field names, which are ASCII. Data from outside, such as
  model ids, never becomes an object key; such maps are arrays of pairs. For ASCII keys,
  RFC 8785's UTF-16 key order equals byte order.
- **Encoding.** No whitespace. Strings are escaped as RFC 8785 requires: `"`, `\`, and
  control characters below U+0020, with `\b \t \n \f \r` and otherwise lowercase
  `\u00xx`; everything else is raw UTF-8. Integers are plain decimal.
- **Parsing stored bytes.** `kwoon verify` and replay parse stored records with a strict
  parser that rejects:
  - duplicate keys;
  - unpaired surrogates and invalid UTF-8;
  - integers outside the range;
  - any number with a fraction or exponent;
  - bytes that do not re-encode to themselves.
- **Result.** The output is valid RFC 8785, so a second implementation can verify the
  chain independently.
- **Size.** The serializer is about 150 lines in the gate crate.

**Reasons.**

- Both RFC 8785 crates are pre-1.0. A defect or behaviour change in either would change
  hashes of records already written. kwoon's encoder only needs the subset its value
  model allows, which leaves out RFC 8785's hard parts (number formatting, UTF-16 key
  sorting).
- Both crates stay in the tests as independent cross-checks. Neither is shipped.
- CBOR is not readable with `jq`, and the off-host copy should be readable without
  kwoon.

**Tests** (in T3):

- the RFC 8785 test vectors that fall inside the value model;
- expected bytes produced by an implementation outside Rust (a Python RFC 8785 library),
  committed as fixtures together with the script and library version that made them;
- property tests that compare with `serde_jcs` and `serde_json_canonicalizer`;
- duplicate keys, unpaired surrogates, invalid UTF-8, and the integer boundaries
  `±(2^53-1)` (accepted) and `±2^53` (rejected).

**Where this differs from precedents.** Research A (O4) suggested picking a library. This
plan writes the encoder itself for the stability reason above, and uses the libraries as
test oracles.

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
  D4 and the policy repository's credential (D6). Fast-forward only, with
  `--force-with-lease=refs/heads/kwoon-checkpoints:<last pushed commit>`.
- **First checkpoint.** When the remote has no `kwoon-checkpoints` branch and the store
  has records, push at once. The first record segment starts at `seq` 1. If the local
  checkpoint branch already contains verified ruling commits for this store, append the
  checkpoint to that branch and preserve its ruling files and ancestry. Create a
  parentless checkpoint only when no local checkpoint branch exists. Use an empty
  expected remote value in the lease, so a branch created meanwhile causes failure.
  Conflicting local or remote store identity prevents the push and raises a question.
- **Recovery (G7).** Read the remote branch. If a commit with the intent's
  `Kwoon-Checkpoint-Tip` is there, record the push as done and do not push again.
- **Cadence (pilot value).** A pass pushes a checkpoint when unpushed records exist and
  the last successful checkpoint push is at least 10 minutes old, whether or not the
  current pass appended records.
  - "Last successful checkpoint push" is the `at` of the latest `action` record that
    confirms a checkpoint push.
  - With no such record, the first-checkpoint rule above applies.
  - `kwoon checkpoint` pushes at once when unpushed records exist.

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
- **Durability.** Before creating ruling objects or updating `kwoon-checkpoints`, require
  effective `core.fsync=committed,reference` and `core.fsyncMethod=fsync` in the policy
  clone. Read both values in the exact Git environment used by those commands. Require a
  supported Git version whose object and reference writers implement these settings.
  Recheck before appending `archive`. A missing or unsupported setting, command failure,
  or failed content/ancestry verification prevents the ruling from taking effect and
  raises a question. Recovery applies the same requirements. Process-kill tests do not
  establish power-loss survival.
- **Verification.** Read the blob back with `git cat-file` and compare bytes. Check that
  the commit is the branch tip or an ancestor of it. Then write the `archive` record.
- **Crash recovery.** List the local branch's commits with that trailer:
  - one commit whose blob verifies, with the durability settings above confirmed: record
    its `archive`;
  - none: commit it;
  - a commit that cannot be verified, or several commits: refuse to archive, record
    `unknown`, and raise a question.
- **What the tests prove.** Kill tests prove recovery after kwoon's process dies. They
  do not prove survival of a power loss. The power-loss claim rests on the fsync
  settings above and on SQLite's `synchronous=FULL`. It is not tested in the MVP.

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
4. Read the clone's objects in process with `gix` (0.89.0) or `git2` (0.21.0) [verified
   2026-10-20]. Neither is safer by default: `gix` decides trust by file ownership, and
   the run and kwoon have the same owner. Either would still need the snapshot and
   confinement below. Not chosen.
5. Git's object reader pointed at the clone's objects as an alternate, in kwoon's own
   environment. Rejected after review. Git reads the alternate's `objects/info/alternates`
   recursively [verified 2026-10-20, git `object-file.c`, `read_info_alternates`]. A run
   can make that file a symlink to the host's token file, and git's error output then
   shows a line of it.
6. **Chosen:** a frozen, checked snapshot of the run's objects, packed by git in a
   credential-free sandbox, then imported into kwoon's publisher repository.

**Mechanism.**

1. **Publisher repository.** kwoon keeps one bare publisher repository per publication
   repository, in its state directory (hidden from runs, D5).
2. **Independent run clones.** kwoon creates each run's clone with
   `git clone --no-local --template=<empty directory> <publisher> <run clone>`.
   `--no-local` uses git's transport rather than a local copy, so the clone shares no
   object file with the publisher through hard links. kwoon creates the branch
   `kwoon/work` at the start head. The task test changes the run's objects and checks
   that the publisher's files are unchanged.
3. **Git environment.** Every git command kwoon runs uses:
   - an explicit `--git-dir`, never a working directory inside a clone;
   - `GIT_CONFIG_NOSYSTEM=1`;
   - `GIT_CONFIG_GLOBAL` pointing at kwoon's own git configuration, which sets
     `core.hooksPath` to an empty directory;
   - `HOME` set to a kwoon directory;
   - `GIT_TERMINAL_PROMPT=0`;
   - the git executable named in host settings (D6).

   A credential is passed only through `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0`, and
   `GIT_CONFIG_VALUE_0` as an `http.extraHeader` in the child's environment. It is never
   in argv or in a file.
4. **Frozen snapshot.** After the run's wrapper and PID namespace have exited, so no
   process of the run is left, kwoon copies the clone's object store into a fresh
   snapshot directory. The copy is made in Rust:
   - Acquire the run-clone root from a trusted parent directory descriptor. Open `.git`
     and every directory component one component at a time with
     `O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`; open accepted files relative to those
     verified descriptors with `O_NOFOLLOW | O_CLOEXEC`, and validate their type and link
     count with `fstat`. Do not follow a `.git` indirection file, symlinked root, or
     symlinked ancestor. Apply the same containment rule to every host-side read of
     run-produced files.
   - only regular files are accepted at these paths:
     - `objects/<2 hex>/<38 hex>`;
     - `objects/pack/pack-<40 hex>.pack`;
     - `objects/pack/pack-<40 hex>.idx`;
     - `refs/heads/kwoon/work`;
     - `packed-refs`;
   - a symlink, device, FIFO, socket, or hard-linked file (link count above 1) anywhere
     on those paths, an `objects/info/alternates` or `objects/info/http-alternates` file,
     any other entry under `objects/`, or a `reftable/` directory rejects the snapshot.
     The action is `failed` and a question is raised.
   - A regular, singly linked `objects/pack/pack-<40 hex>.rev` file is recognized as
     disposable derived metadata. It is not copied into the snapshot or consumed by the
     packing sandbox. A symlink or special file at that path is rejected. All other
     unlisted entries remain rejected.
5. **Final head.** Read from the snapshot's `refs/heads/kwoon/work`, or from that line in
   its `packed-refs`. A symbolic ref, or a value that is not 40 hexadecimal characters,
   makes the final head `unknown`, and nothing is published.
6. **Confined packing.** kwoon runs git inside a bubblewrap sandbox built by the D5
   builder:
   - no network (`--unshare-all`, without `--share-net`);
   - no credential and an empty home;
   - read-only: the snapshot, plus an empty bare repository whose alternate is the
     snapshot's object directory;
   - writable: a throwaway output directory.

   Inside, git runs `pack-objects --revs --stdout` with input `<final>` and `^<start>`,
   writing the pack to the output directory. stdout and stderr go to files in the
   output directory.
7. **Import.** Outside the sandbox, kwoon runs
   `git --git-dir=<publisher> index-pack --strict --stdin` on that pack file. `--strict`
   dies on "broken objects or links" [verified 2026-10-20, `git-index-pack.adoc`]. The
   publisher already holds the start head's objects, because it created the clone.
   From here on, the publisher holds every object, validated.
8. **Ancestry.** In the publisher repository, kwoon checks two things:
   - the spec's rule: the start head is an ancestor of the final head;
   - this plan's rule: the expected remote head, when not `absent`, is an ancestor of the
     final head.

   If either fails, nothing is published: the action is `failed` and a question is
   raised. If either cannot be read, the action is `unknown`.
9. **Push.**

   ```
   git --git-dir=<publisher> push --porcelain --no-verify \
     --force-with-lease=refs/heads/<branch>:<expected head, or empty for absent> \
     <url> <final>:refs/heads/<branch>
   ```

   - The refspec has no `+`.
   - The lease makes the push a compare-and-swap. Git refuses the push unless the
     remote ref equals the expected value, and an empty value means "the named ref must
     not already exist" [verified 2026-10-20, git `Documentation/git-push.adoc`].
   - Step 8 makes every update a fast-forward, so kwoon never sends a non-fast-forward
     update. The lease only adds the condition.

**Reasons.**

- Nothing kwoon runs with a credential reads any path the run controls. The only reads
  of the run's files are kwoon's own descriptor-relative copy and git inside a sandbox with
  nothing to leak.
- Objects pass git's own parser and fsck checks before they are pushed.
- The push runs entirely from a repository whose configuration kwoon wrote.
- The remaining exposure is git parsing untrusted pack data in the import. That is the
  same exposure as any `git fetch` from an untrusted remote.

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

**Choice: bubblewrap with an allowlist.** One builder in `crates/kwoon-run` produces the
command line. Runs, the doctor probes, and D4's confined packing all use it. Its parts:

- **Namespaces.** `--unshare-all`, which unshares IPC, PID, network, and UTS, and tries
  user and cgroup namespaces; the builder also passes `--unshare-user` and
  `--unshare-pid`, so the user and PID namespaces are required, not tried. Runs add
  `--share-net`, for the reason below. The packing sandbox does not.
- **Process.** `--die-with-parent --new-session --clearenv`, then `--setenv` only for
  `HOME`, `PATH`, `LANG`, `TERM`, and the variables the harness needs, from host
  settings.
- **Mounts.** `--proc /proc --dev /dev --tmpfs /tmp`. `--ro-bind` for `/usr` and `/etc`,
  and the usual `/bin`, `/lib`, and `/lib64` links. Two separate writable bindings:
  `--bind <run clone> <same path>`, with the run's working directory there, and
  `--bind <private run home> <home path>`.
- **A private run home.** A directory under the run's own directory, never the
  operator's home. kwoon creates and seeds it before each run:
  - **Provider credential.** A copy of the harness's provider credential, from the
    source host settings names: for example an API key in an environment variable, or a
    credential file [to verify in T1, per harness].
  - **Harness settings.** A minimal settings file that kwoon writes itself. It has no
    hooks, no plugins, and no MCP servers, and it turns the inner sandbox off. For
    Claude Code `-p`, these settings are also passed with `--settings`. For Codex
    `exec`, `--sandbox danger-full-access` [to verify in T1].
  - **Never bound:** the operator's `~/.claude` and `~/.codex`. A writable `~/.claude`
    would let a run add hooks that execute in the operator's next session on the host.
  - **After the run.** After the wrapper and its PID namespace have exited, extract only
    the expected harness-log files through the descriptor-relative containment rule in
    D4. Reject symlinks at the log, any ancestor, or the run-home root; reject special
    files and multiply linked regular files before reading their contents. Treat
    extracted bytes as untrusted data under OQ3. Cleanup must not follow links outside
    the run home.
  - **Seeding is a copy.** Changes the run makes to its home, its settings, or its
    copied credential never reach the host. T1 tests this. If the provider rotates a
    refresh token when the run uses its copy, the host's credential could stop working.
    T1 checks this per harness, and prefers an API-key credential where it does.
- **Descriptors.** At start, kwoon marks every descriptor above 2 close-on-exec
  (`close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`). The wrapper child gets exactly three
  descriptors: stdin from `/dev/null`, and pipes for stdout and stderr.
- **Network.** Runs use `--share-net` because the harness must reach its provider's API.
  A separate network namespace would need a forwarding proxy into it, which is not in
  the MVP. The cost is that host services on loopback, and abstract Unix sockets, can be
  reached from the run. The spec does not guarantee egress protection. The doctor probe
  measures this residual and reports it (below).

The bubblewrap options used exist as described in its manual [verified 2026-10-20,
`containers/bubblewrap` `bwrap.xml`].

Not bound, so absent by construction:

- kwoon's state and configuration directories, including every credential file;
- the operator's home: `~/.config/gh`, `~/.git-credentials`, `~/.ssh`, `~/.gnupg`,
  `~/.claude`, `~/.codex`;
- `/run/user/<uid>`, so a session bus or keyring socket there is not reachable;
- every other variable of the operator's environment, including `SSH_AUTH_SOCK`,
  `GH_TOKEN`, and `GITHUB_TOKEN`.

**`kwoon doctor` items for the wrapper.** All of them use the production builder.

- **`wrapper-starts`**, in three stages. A failure is reported with its stage:
  1. **Namespace setup.** Use the production builder's namespace and process options,
     with a trusted static probe bound read-only into the otherwise empty filesystem.
     Require the user and PID namespaces explicitly; report namespace-creation failures
     separately from probe-execution failures.
  2. **Filesystem setup.** Run that probe with the complete production mount layout,
     including separate writable bindings for the run clone and private run home.
  3. **Harness execution.** Run the configured harness with the same layout and seeded
     home.

  Each stage has a 30-second deadline. T1 must demonstrate a successful real invocation
  before its negative controls. A denied required namespace is a failure, never a skip.
  Optional namespace isolation is reported according to what was actually created.

  The trusted static probe is the host-side static musl `kwoon` executable under test,
  run as `kwoon __probe`. T1's deterministic verification uses its development build
  within the isolated verification context. Normal governed operation and live-negative
  qualification use the released binary required by D2 and D6.
  bubblewrap's exit code and stderr distinguish its own setup errors from the child's
  exit code. The classification is fixed in T1 from observed messages, for example a
  denied user namespace under Ubuntu 24.04's AppArmor restriction (research A).
- **`wrapper-hides-state`.** Run by the probe inside the wrapper. Each check must hold:
  - **Files.** Check every mounted tree, including `/usr` and `/etc`, for reachable
    aliases of the protected files and directories. Direct access and access through an
    alias must both fail. If the mount layout exposes a protected path or inode, refuse
    local runs. The protected set is given to the probe as (device, inode) pairs: the
    forge credential files, the store, `~/.config/gh`, and `~/.git-credentials`.
  - **Mounts.** `/proc/self/mountinfo` lists exactly the mounts the builder declared.
  - **Descriptors.** `/proc/self/fd` holds only 0, 1, and 2.
  - **Sockets.** Connecting to the operator's `SSH_AUTH_SOCK` path and to
    `DBUS_SESSION_BUS_ADDRESS` fails, including abstract addresses.
  - **Environment.** It holds only the variables set by the builder.
- **`wrapper-reachable-services`.** The probe lists the loopback TCP ports and abstract
  Unix sockets it can connect to. This item reports them; it does not fail on them. A
  socket that `wrapper-hides-state` names makes that item fail instead.
- **`harness-config-clean`.** Every file seeded into the run home is scanned:
  - for GitHub token prefixes: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`;
  - for the exact bytes of each of kwoon's forge credentials.

  A match makes the item fail, and local runs are refused (G1).
- **`harness-starts-in-wrapper`**, per harness. One minimal prompt runs inside the
  wrapper with the inner sandbox off. The harness exits 0 and its log shows a model
  response, which proves it reached its API.

**Reasons.**

- An allowlist hides secrets kwoon does not know the paths of, so G1 for runs kwoon
  starts does not depend on a list of known secret paths.
- A private run home also keeps a run from changing the operator's own harness setup.
- Bubblewrap is what both mature harnesses already use on Linux (research A).

**Where this differs from precedents.** Codex and Claude Code configure bubblewrap to
protect the host from the agent's writes. kwoon's wrapper hides the operator's
credentials from the agent's reads, so it starts from an empty home instead of the real
one.

### D6. Supporting choices

#### Forge client

- **Transport.** Built on `ureq` 3.4.2 [verified 2026-10-20], synchronous, with `rustls`
  and `serde_json`.
  - Typed functions for exactly the REST endpoints the spec uses.
  - GraphQL as a plain POST with constant query strings and typed response structs.
  - The two mutations are `markPullRequestReadyForReview(input: {pullRequestId})` and
    `convertPullRequestToDraft(input: {pullRequestId})` [verified 2026-10-20,
    `octokit/graphql-schema` `schema.graphql`].
- **Deadlines.** Every external call has one:
  - HTTP: 10 seconds to connect, 60 seconds per request;
  - git subprocesses: 300 seconds, after which kwoon kills the process group;
  - bubblewrap setup stages: 30 seconds;
  - a run: the harness timeout in host settings.

  An action whose call hits its deadline after sending is `unknown`, and its recovery
  rule applies.
- **Options weighed:**
  - `octocrab` (0.54.5): admits gaps in coverage and has no wrapper for the two mutations
    (the maintainer's note, research A);
  - `graphql_client` (0.16.0): compile-time checks against a schema. It can work from a
    minimal schema with only the types kwoon uses, but that schema would be
    hand-maintained and could drift from GitHub's without a test noticing.

  The recorded-response tests need exact control of requests and of read completeness,
  which a general client hides. The live tests are what prove the queries. Synchronous
  code is enough for one pass at a time. The rust-lang/bors maintainer reported slow
  incremental builds with async code (research A).

#### Complete reads, per endpoint

"Every `next` link was followed" is necessary but not sufficient. Each endpoint has its
own rule. A read that does not meet its rule is incomplete, and every caller turns an
incomplete read into `unknown`.

| Read | Endpoint | Complete when | Known cap and handling |
|---|---|---|---|
| Workflow runs of the policy workflow on one commit | `GET /repos/{o}/{r}/actions/workflows/{file}/runs?head_sha=…` | Every page read without error, and the number of distinct runs collected equals `total_count` on every page. A query whose `total_count` is 1,000 or more is complete only through the partitioning in the next column. | Filtered run lists return at most 1,000 results when `head_sha`, `branch`, `event`, `status`, `created`, `actor`, or `check_suite_id` is used [verified 2026-10-20, GitHub REST description]. kwoon then partitions the query by `created` ranges, halving each range until every part is below 1,000 and the parts' counts add up. If that does not converge, the read is `unknown`. |
| Workflow runs on the target branch (G13 start) | the same endpoint, with `branch=…` and `event=workflow_dispatch` | The same rules. | The same cap and partitioning. |
| Jobs of one run attempt | `GET /repos/{o}/{r}/actions/runs/{id}/attempts/{n}/jobs` | Every page read; the number of jobs collected equals `total_count`. | None documented [to verify in T6]. |
| Pull requests in the recorded governed repository (recovery of “Open the draft pull request”) | `GET /repos/{o}/{r}/pulls?state=all`, in the recorded governed repository, with neither a `head` nor a `base` filter. | Every page read; the last page has no `next` link. Whether this endpoint caps its results is [to verify in T14]; until a completion rule and its negative case exist, the read is incomplete. | Not the search API. Search results can be capped and its index can lag behind writes (not verified in this revision), so a pull request kwoon just opened could be missing and kwoon would open a second one. The list endpoint avoids depending on either. **Classification after the read:** kwoon looks for the intent marker in every returned pull request and compares each marker match's head repository, head branch, base repository, and base branch with the intent. One marker match with all four equal: already opened. A marker match with any endpoint different (for example a base that GitHub retargeted after a stacked merge): conflicting evidence, so `unknown`, a question, and no new pull request. Several marker matches: record `unknown`, raise a question, and open no replacement. No marker match on a complete read: open it. An unreadable or incomplete search: record `unknown`, raise a question, and open no replacement. **Endpoint changes:** Find the intent marker before comparing endpoint identities. A renamed head branch or a transferred or renamed head repository must not exclude a marker match from the recovery search. Apply the conflicting-evidence rule when any recorded endpoint identity differs. If the required search cannot be read completely, record `unknown`, raise a question, and open no replacement. |
| Comments on one pull request (handoff comment marker) | `GET /repos/{o}/{r}/issues/{n}/comments` | Every page read; the last page has no `next` link. | [to verify in T15] |
| Review-request history | the pull request's timeline events | Every page read; the last page has no `next` link; T15 checks for a cap. | [to verify in T15] |

Each endpoint has positive completeness tests and negative tests using evidence that
endpoint actually exposes. Do not invent `total_count` for array endpoints. Before
accepting an endpoint whose cap behavior remains unverified, establish its completion
rule and add the corresponding negative case; otherwise report the read as incomplete.

#### Forge credentials

A fine-grained personal access token reaches resources of a single user or
organization. It also does not support contributing to public repositories where the
user is not a member, contributing to repositories where the user is an outside or
repository collaborator, or calling the Checks API [verified 2026-10-20, GitHub docs
source, "Managing your personal access tokens", "limitations"]. So one token cannot
serve every setting.

- **Credential sets.** Host settings name each credential by id, with its file, its kind
  (fine-grained or classic), and the resource owner it was made for. Each file has mode
  `0600`, lives in kwoon's configuration directory, and is never bound into a wrapper.
- **Routing.** Host settings map every (repository, operation) pair kwoon uses to one
  credential id. The forge client picks the credential from that map and nowhere else.
  `kwoon doctor` refuses to start if a pair the policy needs has no credential, and
  reports which pairs use a classic token.

  Routing covers Git fetches and pushes, pull-request reads and writes, comments,
  reviewer requests and history, Actions reads, and conditional G13 re-run/dispatch
  operations. Fork pushes use a fork credential with contents write permission. Policy
  fetches and checkpoint pushes use the policy repository's route. Upstream
  re-run/dispatch operations receive an explicit route only where the account has the
  required rights; otherwise G13 is reported as tracked, not guaranteed. Missing routing
  for an optional unavailable G13 operation does not prevent unrelated operation. No
  operation falls back to another credential.

| Setting | Repositories and operations | Credential |
|---|---|---|
| The operator's own repository | push branches, open and change pull requests, read CI, re-run, dispatch | A fine-grained token for that owner: contents, pull requests, and actions (write); workflows if T14 shows it is needed |
| The policy repository, wherever it lives (OQ7) | fetch the policy, push checkpoints | A separate repository: a fine-grained token for its owner with contents (write); under the same owner as the governed repository, the same token may list both repositories. The governed repository itself: the governed repository's route carries these operations, and the policy file is a policy-layer path there (spec, "Policy v1"). |
| A fork contribution | push to the fork | A fine-grained token for the operator's account, limited to the fork |
| A fork contribution | in upstream: open the pull request, mark ready, convert back, request reviewers, comment, read CI | A classic token with `public_repo` (or `repo` for a private upstream), because fine-grained tokens cannot contribute there. Used only for these operations. |
| A company repository, operator in the organization | as for the own repository | A fine-grained token for that organization; the organization may require approval |
| A company repository, operator an outside collaborator | as for the own repository | A classic token with `repo`, for the same reason |

CI is read through the Actions API (workflow runs and jobs), never the Checks API, so a
fine-grained token works for it [to verify in T2].

T2 proves the fork and outside-collaborator rows in slice 1, next to the owned-repository
proof.

#### Policy file and host settings

- **Policy file.**
  - Format: TOML, parsed with `deny_unknown_fields`. Every option maps to a Rust enum or
    newtype, so the closed value sets are matched exhaustively.
  - Location: `kwoon-policy.toml` at the policy repository's root.
- **Host settings.** A TOML file in kwoon's configuration directory, not in the policy.
  It holds:
  - paths, and the git executable;
  - the credential sets and routing above;
  - harness commands and the provider-credential sources for run homes;
  - each governed repository's publication repository and head-branch pattern;
  - the requested mode, shadow or live, per governed repository (see "Mode switch").

  The settings of an isolated verification context (below) are a separate file, with
  separate credentials, its fixture allowlist, and its extra TLS roots.

#### Isolated verification context (decided on #15, OQ10; spec revision in #16)

**Isolated verification context.** Test execution uses a separate state directory whose
test classification is immutable, separate host settings and credentials, and an
explicit allowlist of fixture repository identities and endpoints. A test store cannot
be reopened as a normal store or contribute records to normal shadow-promotion evidence.

Deterministic tests may run the development binary through the production command entry
point. The released-binary check retains its truthful result; the approved test context
permits execution without representing that result as verified. Live-negative
qualification uses the released binary.

Extra TLS roots apply only to the explicitly configured fixture endpoints and test-proxy
connections, including Git subprocesses. Normal clients and repositories never inherit
them.

Test classification does not bypass the mode-switch state machine. Tests of
already-authorized actions may seed valid request, qualification, question, answer, and
effective-mode records in the isolated fixture store. Mode-switch tests create and
validate those records through the production commands.

All test records remain identifiable as test evidence. The live-negative report may be
used as the spec's required test result, but synthetic gate decisions and operator
comparisons never count toward the ten real shadow pull requests.

Required negative controls (T2 for TLS scope, T4 for the store and allowlist, T21 for
promotion and mode):

- reopening a test store as normal;
- using test observations toward real promotion;
- contacting an unlisted repository;
- leaking test CA trust into a normal client;
- activating live without a valid confirmation.

This exception changes spec text, so it is settled only once the spec revision in #16
merges (see "Blocked tasks").

#### Mode switch (decided on #15, OQ5)

No new record kind is introduced. Record requested-mode changes by comparing host
settings with the last recorded request, not with the effective mode. Record the initial
shadow state. Repeated reads of an unchanged request write nothing.

A live request raises one question only after a verified promotion capability identifies
ten consecutive distinct pull requests whose recorded decisions matched the operator's,
including at least three passes, and a passing result for every required live negative
test. Missing, failed, or unknown evidence prevents promotion.

The question names the request record and the exact qualification evidence. A
withdrawal, replacement request, changed qualifying facts, or deadline expiry lapses it.
An answer to a lapsed question cannot activate live.

A valid answer choosing live causes a new effective-mode `capability` record naming the
answer in `authorized_by`. Retain the capability schema's status, proving test, and
validity fields; requested and effective mode are explicit item-specific fields.
Switching back to shadow writes a new effective-mode record immediately and invalidates
pending live requests.

Every gate record includes the effective-mode record among its inputs. A mode change
therefore creates a new gate record even when the other inputs and decision are
unchanged.

T4 records the initial requested shadow mode and effective shadow-mode capability during
initialization, retaining the required capability fields. T10 includes that
effective-mode record in every gate evaluation. T5 uses only modes already authorized in
its isolated fixture store. T21 builds the subsequent request lifecycle and confirmed
mode transitions. Until T21 is complete, normal governed repositories remain in shadow
mode.

#### Workspace layout

- `crates/kwoon-gate`: records, canonical encoding, policy parsing, and the evaluator.
  These judge.
- `crates/kwoon-store`, `crates/kwoon-forge`, `crates/kwoon-run`.
- `crates/kwoon`: the binary.

Policy-layer paths for kwoon's own repository, when its policy is written:
`.github/workflows/`, `crates/kwoon-gate/`, `Cargo.lock`, `rust-toolchain.toml`, and the
policy file if it is in this repository. `Cargo.lock` is included because a dependency
update can change what the gate decides.

#### Test harness

Every deterministic test runs the built `kwoon` binary with a temporary state directory
and host settings. It asserts the records written, the decision, and the exit code
(`AGENTS.md`, "Checks and gates").

- **Fake forge.** A test server under `tests/support/` serves REST and GraphQL over HTTPS
  from scenario fixtures, with a test CA given as extra TLS roots for its endpoint in the
  isolated verification context.
  It keeps state (draft flag, reviewers, timeline events, workflow runs) and records
  every write.
- **HTTP kill seam.** The harness gives the fake forge the pid of the `kwoon` process. A
  scenario can say "after applying write W, SIGKILL kwoon, then drop the connection".
- **Git kill seam.** Host settings name the git executable. A test points it at a
  wrapper script that runs the real git with the same arguments and environment. When
  the scenario's matching command succeeds (for example a push, `commit-tree`, or
  `update-ref` on `kwoon-checkpoints`), the script sends SIGKILL to kwoon before
  returning. This seam does not depend on hooks, which D4 turns off everywhere.
- **Live kill seam.** A loopback HTTPS proxy terminates TLS with a test CA, which the
  release binary accepts only as extra TLS roots for the test-proxy connections of the
  isolated verification context. It re-originates
  TLS to `api.github.com` and `github.com`, waits for GitHub's response to the
  scenario's write, and kills kwoon before forwarding it. Git uses the same proxy and CA
  through `http.proxy` and `GIT_SSL_CAINFO`, set by kwoon from host settings.
- **What every kill test asserts.** The production binary has no fault-injection code.
  Every kill test asserts three things:
  1. the child died from SIGKILL, read from its wait status;
  2. the external action happened, read from the fake forge's write log, the remote's
     refs, or GitHub;
  3. before the restart, the store held the action's intent without a confirmation.

  Then it asserts the recovery.
- **Fake harness.** A script stands in for the harness inside the wrapper. It writes
  commits, prints verdicts, rewrites its own log, and runs the probes of D5.
- **Recorded responses.** T2 adds a capture script. Its responses are scrubbed of user
  names, email addresses, and tokens before commit. A CI check rejects fixtures that
  contain an email address or a token prefix (`AGENTS.md`, ground rule 1).

## Answers to the plan open questions on #4

From comment 6297457433, with the decisions from the review on #15. The two items #13
deferred (pending runs, the breaker unit) are Q1 and Q2.

### Q1. Pending runs and incomplete reads (adopted on #15)

**Rule.**

- **Observed run set.** For each check id and commit, kwoon reads the complete list of
  runs of the policy's workflow file on that commit (D6, "Complete reads").
- **Pending run.** A run in that list is pending when its status is not `completed` and
  it has no usable completed result for the configured job.
- **Result and completeness.** A proven current failure remains `failed`; retain and
  report any incomplete observations. Otherwise, an incomplete read or an unknown run
  result makes the combined result `unknown` and raises a question. Otherwise, a known
  pending run prevents a combined pass and contributes no red. With no pending run,
  apply the spec's existing attempt and combination rules.
- **Consequences.** For a required check, a pending run makes gate step 4 fail with the
  reason "pending: <run ids>" and raises no question; the next poll re-evaluates. For
  acceptance, the commit stays `not finished`. An `unknown` result fails step 4 (G20),
  and for acceptance means `not finished` with a question.
- **Persistence for replay.** Every affected `gate` and `acceptance` record, including
  `passed` and `failed` acceptance outcomes, stores the observed run set and completeness
  result. Identify the repository, commit, check, workflow, run ids, observed statuses,
  and input `check` records. Gate steps 4 and 8 retain their respective observations.
  Replay uses only these recorded inputs.
- **Evidence kinds.** A `check` retains the job-scoped and workflow-scoped forms defined
  by the spec; pending observations do not create completed-check evidence.

**Acceptance cases** (deterministic; T6 for required checks, T17 for acceptance).

1. On one head, a push run succeeded and a pull-request run is in progress: step 4
   fails with "pending", and no question is raised. When the run later succeeds, the
   gate passes.
2. One run failed and one is pending: the combined result is `failed`.
3. An acceptance commit has one successful run and one pending run: `not finished`, and
   no red.
4. The second page of the run list returns an error: `unknown`, the gate fails, and a
   question is raised.
5. A capped initial query is partitioned into complete, non-overlapping ranges. A
   fixture with more than 1,000 runs whose partitions each fall below the cap is
   complete and includes every distinct run id. A saturated range that cannot be split,
   inconsistent totals, duplicate ids hiding missing runs, or an exhausted read deadline
   gives `unknown`. Persist and replay both outcomes.
6. Replay of each `gate` and `acceptance` record gives the same result from the stored
   set, with the fake forge stopped.

### Q2. Breaker counting unit (decided on #15: failed target commits)

**Rule.** The red count is the number of distinct `(repository, target commit)` pairs
that have at least one failed attempt of a policy acceptance job, in any run of the
acceptance workflow.

- Each pair counts once. Its time is the forge completion time of the earliest failed
  attempt on that commit.
- A re-read, another failed job, another failed run, or a later attempt on the same
  commit adds no red and does not move that time.
- A later successful attempt does not erase the historical failure.
- Missing or invalid identity or completion time makes the count `unknown` (G20).

**Reason.** The breaker measures bad target revisions, and since #13 runs are combined
per commit. One bad commit with a push run and a manual run is one bad revision.

**Spec conformance.** The spec says "distinct `(repository, workflow run id)` pairs". T18
remains blocked until a spec revision consistently adopts distinct
`(repository, target commit)` pairs in the counting rule, pilot-value wording, and G14
acceptance row. The revised acceptance row must state that two failed runs on one target
commit count once and failures on two distinct target commits count twice. Preserve
earliest-failure timing, deduplication, replay, and unknown-input behavior. That
revision is #16.

**Acceptance cases** (deterministic, T18).

1. One commit has a failed push run and a failed manual run: one red, and the breaker
   stays off.
2. A second commit fails within the window: two reds, and the breaker turns on.
3. One run fails across two attempts, and is read twice: one red.
4. Replay gives the same count and breaker state.

### Q3. Gate step 8 when acceptance is `unknown` or nothing has finished yet (confirmed on #15)

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
- **Red-fix approvals.** An operator approval marking the pull request as the fix for
  the red target overrides a red step 8 only. It never overrides an `unknown` step 8
  (confirmed on #15, OQ4).

**Reason.** G20 forbids guessing. A pending commit is not a guess, but an `unknown` one
is.

**Acceptance cases** (deterministic, T10).

1. The newest commit is `unknown` and an older one `passed`: step 8 is `unknown`, the
   gate fails, and a question is raised.
2. The newest commit is pending and an older one `passed`: step 8 passes.
3. The newest commit is pending and an older one `failed`: red.
4. No finished result since the anchor: `unknown`, and a question.
5. A red-fix approval overrides case 3. It does not override cases 1 and 4: the gate
   still fails with `unknown`.

### Q4. Incident baseline (adopted on #15)

**Rule.** The question raised for a red target lists the merges on first-parent target
commits after the newest commit whose combined acceptance result is `passed`, up to and
including the failed commit. A run's success on a commit whose combined result is
`failed` never sets the baseline. If no commit since the audit anchor has a combined
`passed`, the list starts at the audit anchor set with `kwoon init`, the anchor
excluded.

**Spec conformance.** The spec says "since the last passing run". This plan reads
"passing" as the combined result, the same reading the spec applies everywhere else
since #13.

**Acceptance cases** (deterministic, T17).

1. C1 is combined `passed`. C2 has one successful run and one failed run, so C2 is
   combined `failed`. C3 fails. The question for C3 lists the merges in C2 and C3,
   starting after C1, not after C2.
2. No commit after the anchor A has passed. C1 and C2 follow A, and C2 fails. The
   question lists the merges in C1 and C2.

## Slices and tasks

### How the slices meet the spec's delivery order

The spec makes the first slice "one end-to-end handoff decision on kwoon's own
repository in shadow mode". Task #14 requires the first slice to prove three things
first, in a set order. Slice 1 does both:

- It starts with the three proofs: T1, T2, and T5. T3 and T4 come between T2 and T5,
  because a kill test needs a store and every record needs a policy (G18).
- Milestone **M1** follows them.
- It ends with the shadow decision: T6 to T12, and T22.

Task ids are stable across revisions of this plan. T22 was added after the review of
`24a470c`.

### Milestone M1: Rust go/no-go

M1 is reached when T1, T2, and T5 are merged and their reports are on #4. Every task
after T5 lists M1 in its dependencies. T1, T2, and T5 need the isolated verification
context, so M1, and every task gated by it, waits for the spec revision in #16 (see
"Blocked tasks").

**Proof 1 (T1).** It fails in Rust if any of these does:

- a Rust process cannot start bubblewrap with the builder of D5;
- it cannot keep the process tree under control (die with parent, kill on timeout,
  descriptors closed);
- a harness that works in bubblewrap cannot be started from Rust.

**Proof 2 (T2).** M1 requires successful draft → ready → draft transitions in the owned,
fork-contribution, and outside-collaborator fixtures, plus the credential-routing proofs
assigned to T2. Each credential is tested only for its assigned operations; the policy
credential is not required to perform pull-request mutations. A refused required
operation leaves M1 unmet and is reported immediately. Later work does not start until
the fixture is corrected or the maintainer revises the supported design. A permission
limitation is not evidence that Rust should be replaced by Go.

**Proof 3 (T5).** It fails in Rust if the kill-and-restart test cannot run through the
built binary, with its three assertions.

If one of them fails, the work stops at that task:

1. The implementer comments on the task issue and on #4 with the failing step, the
   evidence, and whether the failure is specific to Rust. A failure that would also
   happen in Go, such as bubblewrap blocked by the host, is not a reason to switch.
2. No later task starts.
3. A Go fallback needs a new plan. This plan is frozen once merged (`AGENTS.md`).

### Blocked tasks

| Blocked by | Tasks | Released when |
|---|---|---|
| OQ3, harness log writable by the run (decided by #18) | T7, and every task that depends on it: T10, T12, T13, T14, T15, T16, T17, T18, T19, T20, T21 | #18's spec revision merges. |
| OQ7, where the policy repository lives (held for the maintainer) | T12, and through T12: T21 | The maintainer decides OQ7. |
| The consistent breaker-unit spec revision defined in Q2 (#16) | T18, and T21 | The spec revision in #16 merges. |
| OQ10, isolated verification context | T1, and every task that depends on it, directly or through M1 | The spec revision in #16 merges. |

Every task now depends directly or transitively on #16. The OQ3 and OQ7 holds and all listed task dependencies remain in effect.

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
`cargo test --locked` in CI. These are not repeated below. Every kill test makes the three
assertions of D6, "What every kill test asserts".

---

### Slice 1: proofs, then the shadow decision

#### T1. Workspace, CI, and the local-run wrapper (proof 1) — blocked by #16

- **Builds:**
  - the cargo workspace of D6 and the verification-context bootstrap needed by early
    commands: a separate state directory with immutable test classification, separate
    test settings and credentials, and the fixture repository and endpoint allowlist;
  - `rust-toolchain.toml`;
  - a CI workflow for fmt, clippy, test, and the static musl build. The task issue must
    say explicitly that it may add `.github/workflows/`;
  - `crates/kwoon-run`'s wrapper builder (D5), with the private run home and its
    seeding, descriptor closing, and the three-stage start;
  - `kwoon doctor` printing the wrapper items. Capability records come in T9.
- **Acceptance rows:**
  - G1 [credential and store absent inside the wrapper];
  - G4 [writing to the store fails from inside the wrapper]: the state directory is
    absent there. T3 repeats it with a real store.
- **Check first:**
  - Still to verify 7: the wrapper starts with each supported harness, and the harness
    reaches its API.
  - [plan] the options that turn off each harness's inner sandbox, and the minimal
    seeded files each harness needs in a private home;
  - [plan] whether a run's use of a copied provider credential rotates or invalidates
    the host's copy;
  - [plan] a CI runner on which the namespace stage succeeds. A denied namespace probe is
    a failure, never a skip (decided on #15, OQ9). If GitHub's `ubuntu-24.04` runner
    denies it, T1 shows another runner that works, and the CI workflow uses it;
  - [plan] a static musl build makes an HTTPS request with `rustls`.
- **Verification:**
  - Deterministic, `cargo test --test wrapper`, with the fake harness. Each item of
    `wrapper-hides-state` is asserted:
    - the (device, inode) check;
    - the mount list;
    - the descriptor list: kwoon is started with an extra open descriptor to a canary
      file, and the probe cannot see or read it;
    - the sockets: the test starts an SSH agent and a session bus listener;
    - the environment.
  - `wrapper-reachable-services`: the test starts a loopback TCP listener and an
    abstract Unix socket, and asserts the report lists both.
  - Run-home isolation: the fake harness changes its settings, adds a hook file, and
    edits its copied credential. The host's `~/.claude` and `~/.codex` fixtures and the
    credential source are byte-identical afterwards.
  - Include an intentionally exposed alias of a protected canary in an otherwise allowed
    mount. The probe must fail. With the correct layout, the private home must be
    writable inside the wrapper while the host's harness homes remain unchanged.
  - Test symlink substitutions at `.git`, `objects`, an object fan-out directory,
    `refs/heads`, the run-home root, a log directory, and the log itself. The host-side
    fixture supplies canary targets outside the permitted roots. Extraction must reject
    each substitution without opening the canary, publishing, or including its bytes in
    output, diagnostics, records, or questions. Keep the existing leaf-file,
    special-file, alternates, and hard-link cases. (T1 covers the run-home, log
    directory, and log cases; T22 covers the clone cases.)
  - Negative cases:
    - a seeded file containing a token prefix fails `harness-config-clean`;
    - after a successful real invocation of all three stages, each `wrapper-starts`
      stage is made to fail on its own (a required namespace denied, a missing bind
      source, a missing harness executable). Each is reported with its stage, and
      `kwoon doctor` exits 1;
    - a harness that outlives its timeout is killed with its whole process tree.
  - Live, run by the operator on the host:
    - `kwoon doctor --harness claude-code` and `--harness codex` start each real harness
      with `-p` or `exec` inside the wrapper, with the inner sandbox off and a private
      home;
    - the output and the probe report are posted on the task issue.
- **Depends on:** the spec revision in #16, for the isolated verification context.

#### T2. GitHub client, credentials, and the two GraphQL mutations (proof 2) — blocked by #16

- **Builds:**
  - `crates/kwoon-forge`:
    - credential sets and routing (D6);
    - the REST and GraphQL transport, with deadlines;
    - per-endpoint completeness (D6, "Complete reads");
  - reading a pull request (node id, `isDraft`, head, base, author);
  - `set_draft_state(pr, ready | draft)`. It reads the state first and sends the
    mutation only if the state differs, so the target state is idempotent;
  - the fake forge and the response-capture script of D6;
  - the isolated context's transport enforcement for the forge client and Git
    subprocesses, including the fixture allowlist and scoped TLS roots, using T1's
    bootstrap;
- **Acceptance rows:** the forge half of G7 [mark ready] and G7 [back to draft]: the
  target state is idempotent, so setting it again changes nothing. T5 and T15 add the
  kill and recovery halves.
- **Check first:**
  - Still to verify 1, for all three settings:
    - who may mark a pull request ready, or convert it back, in the operator's own
      repository, in an upstream repository the operator contributes to from a fork,
      and in a repository where the operator is an outside collaborator;
    - which plans and repository kinds offer draft pull requests.
  - [plan] fine-grained tokens can read workflow runs and jobs through the Actions API.
  - The maintainer provides the live fixtures: the test repository, an upstream
    repository the operator is not a member of, and a repository where the operator is
    an outside collaborator.
- **Verification:**
  - Deterministic, `cargo test -p kwoon-forge`, with the fake forge:
    - both mutations;
    - "already in the target state" sends no mutation;
    - a GraphQL `errors` array, HTTP 401 and 403, and a rate-limit response each give a
      typed error, never success;
    - a request past its deadline gives a typed timeout;
    - an operation routed to a credential that is missing from host settings sends
      nothing;
    - per row of "Complete reads", positive and negative completeness tests using the
      evidence that endpoint exposes (D6);
    - Use distinct synthetic credentials for publication, governed, and policy
      repositories. For every routed operation, assert the exact credential selected at
      the HTTP or Git process boundary. A missing route sends nothing; a refused
      credential causes no fallback. Add operator-run transport proofs of fork push
      access, policy fetch/checkpoint-branch push access, and Actions reads, alongside
      the three draft-state proofs.
    - the isolated verification context's extra TLS roots: a client for a fixture
      endpoint trusts the test CA; a normal client and a Git subprocess for a
      non-fixture endpoint do not.
  - Live, run by the operator: `cargo test --test live_github -- --ignored`. It goes
    draft → ready → draft, reading `isDraft` after each step:
    - in the owned test repository, with a fine-grained token;
    - in the upstream repository, on a pull request from the operator's fork, with the
      classic token routed for upstream;
    - in the outside-collaborator repository, with the classic token.

    The captured, scrubbed responses become the fixtures.
  - M1 requires successful draft → ready → draft transitions in the owned,
    fork-contribution, and outside-collaborator fixtures, plus the credential-routing
    proofs assigned to T2. Each credential is tested only for its assigned operations;
    the policy credential is not required to perform pull-request mutations. A refused
    required operation leaves M1 unmet and is reported immediately. Later work does not
    start until the fixture is corrected or the maintainer revises the supported design.
    A permission limitation is not evidence that Rust should be replaced by Go.
- **Depends on:** T1; the spec revision in #16, for the test transport.

#### T3. Event store — blocked by #16 (through T1)

- **Builds:**
  - `crates/kwoon-store` (D1): the lock and its bounded scopes, the head row, and the
    consistency check before every write;
  - the canonical encoder, the strict parser, the record envelope, and the hash chain
    in `crates/kwoon-gate` (D3);
  - schema validation for every record kind of the spec;
  - `kwoon verify` for the local chain;
  - a machine-readable summary printed by every write command after commit, listing the
    ids of the records it committed;
  - binding each event store to its context's immutable classification, preserving
    test-evidence identity and refusing to reopen a test store as normal;
- **Acceptance rows:**
  - G18 [a record without a policy reference is rejected];
  - G6 [an edited old record is found]: here against the local chain; T13 adds the
    pushed checkpoint;
  - G4 [store side; degraded]: the raw-writer test;
  - G5: the `derived.db` rebuild mechanism. The gate-decision part is in T10.
- **Check first:** [plan] `flock` on `store.lock` survives the store's own SQLite file
  being opened and closed in the same process.
- **Verification:**
  - Deterministic, `cargo test -p kwoon-gate canonical`: the encoding tests of D3.
  - Deterministic, `cargo test --test store`, asserting each of these:
    - an edited record is reported with its `seq`;
    - a record missing `policy` is rejected with a non-zero exit code;
    - **raw writer.** The test makes both kinds of raw write with `sqlite3`. After the
      inconsistent raw insert, `kwoon verify` exits non-zero. T4 additionally proves that
      the next `kwoon policy accept` appends nothing. T5 proves that the next
      `kwoon poll` appends nothing and makes no forge call. The well-formed raw append
      remains accepted as the documented authenticity residual.
    - two `kwoon` writers serialize on the lock;
    - a command that waits more than 120 seconds for the lock exits non-zero and writes
      nothing.
  - **Crash test with an oracle.** 300 iterations. Each starts a write command and kills
    it with `kill -9` at a random delay. The oracle is the set of record ids that
    commands printed as committed before they died. After each restart:
    - `kwoon verify` passes;
    - every record in the oracle is present;
    - every record present is either in the oracle or was written by the killed
      command's last, unacknowledged transaction;
    - no record is torn.
  - Negative: a value outside the value model (a float, a non-ASCII key) is a
    serialization error, never a silent conversion.
- **Depends on:** T1.

#### T4. Policy v1, host settings, and init — blocked by #16

- **Builds:**
  - the policy schema for every option in "Policy v1", as closed Rust types;
  - `kwoon policy accept <commit>`, which reads the file with the git environment of D4,
    runs the floor check, and writes the `policy` record;
  - host settings loading, including the credential sets and the requested mode;
  - policy and initialization integration with the isolated verification context built
    in T1–T3; T4 runs the store-separation and fixture-allowlist acceptance controls;
  - `kwoon init <repository> --anchor <commit>`, including the initial requested-mode
    and effective shadow-mode records described in D6;
- **Acceptance rows:**
  - G17 [a commit that lowers a floor, or removes a policy-layer path, is refused; a
    merged policy change has no effect until accepted];
  - G8 [a policy that exists only in the store and not in git: `kwoon doctor` refuses
    to start]. T4 adds the check; T9 makes it a capability item.
- **Check first:** the spec revision in #16 has merged, for the isolated verification
  context.
- **Verification:**
  - Deterministic, `cargo test --test policy`, with fixture policy repositories. The
    following are refused with a non-zero exit code, and no `policy` record is written:
    - an unknown key;
    - a value outside a closed set;
    - a review count below 1;
    - a round limit below 1;
    - a removed policy-layer path;
    - missing acceptance checks (G13).
  - Positive cases, accepted with a `policy` record:
    - a review count lowered from 2 to 1, which the spec allows ("1 or more");
    - a policy-layer path list that only grew.
  - A merged but unaccepted commit leaves the old policy in effect.
  - After T3's inconsistent raw insert, the next `kwoon policy accept` appends nothing
    and exits non-zero.
  - Isolated verification context: reopening a test store as normal is refused, and a
    command naming a repository or endpoint outside the fixture allowlist contacts
    nothing.
  - Initialization through the production command creates the initial requested shadow
    mode and effective shadow-mode capability. Repeating initialization with an
    unchanged request creates no duplicate mode records.
- **Depends on:** T2, T3; the spec revision in #16, for the isolated verification context.

#### T5. Action journal and pull-request state audit, with the kill test (proof 3) — blocked by #16

- **Builds:**
  - `kwoon poll` as one pass under the store lock, with its deadline (D1);
  - `action` records with stable intent ids, and start-of-pass recovery;
  - the "mark ready" and "turn back into a draft" recovery rules of "External actions";
  - the first audit rule. A pull request opened by the operator's forge account that is
    ready for review with no `handoff` for its current head is turned back into a draft
    in live mode. In shadow mode kwoon records what it would do. In both modes it writes
    an `unaccounted` audit and a `question`;
  - `kwoon questions`.

  The live-mode kill test runs in the isolated verification context, with live mode
  seeded as already authorized (D6). The mode-switch state machine is built in T21.
- **Acceptance rows:**
  - G7 [back to draft: killed after the forge accepted, before recording; after restart,
    recorded once, nothing repeated];
  - G5 [a pull request marked ready without a handoff goes back to draft and a question
    is raised], deterministic. The live case is in T16 and T21.
- **Check first:** none beyond T2's.
- **Verification:**
  - Deterministic, `cargo test --test kill_back_to_draft`:
    1. The fake forge kills kwoon after applying `convertPullRequestToDraft`.
    2. The test makes the three kill assertions: SIGKILL in the wait status; the
       forge's write log has the mutation; the store has the intent without a
       confirmation.
    3. The restarted `kwoon poll` reads the draft state, completes the existing intent,
       and sends no second mutation (the forge's write log still has one entry).
    4. Exactly one `audit` and one `question` exist.
  - Negatives:
    - the pull request state cannot be read after the restart: the intent becomes
      `unknown`, a question is raised, and no mutation is sent;
    - shadow mode: no mutation at all;
    - after T3's inconsistent raw insert, the next `kwoon poll` appends nothing and
      makes no forge call.
- **Depends on:** T2, T4; the spec revision in #16.

#### T6. CI evidence: `check` records and combined results

- **Builds:**
  - reading workflow runs and jobs for a commit, restricted to the policy's workflow file
    and job, with the completeness rules of D6;
  - the provenance rule: the exact commit, the named workflow and job, no plain commit
    statuses;
  - job-scoped and workflow-scoped `check` records;
  - the attempt rule;
  - the combined result of "Several runs on one commit", plus Q1's pending rule and its
    persisted observed run set.
- **Acceptance rows:**
  - G20 [a required check's result cannot be read];
  - G20 [two runs on one head, one failed and one passed, in either finishing order];
  - G20 [one passed and one unrecognized gives `unknown`];
  - G14 [`neutral`, `skipped`, and `timed_out` are failed; pending is not finished;
    missing or unrecognized is `unknown`]: the classification part. T17 covers the
    acceptance part;
  - Q1 cases 1, 2, 4, 5, and 6.
- **Check first:**
  - Still to verify 6: which commit a pull-request workflow tests (the head, or a test
    merge), and whether the forge reports it in the job or run data.
  - Still to verify 2, in part: whether upstream CI runs at all on a fork's pull request,
    and what the run list shows when it does not.
  - [plan] whether the jobs endpoint caps its results.
- **Verification:**
  - Deterministic, `cargo test --test checks`. Fixtures:
    - a job with the right name from another workflow: no record;
    - a commit status with the check's name: no record;
    - attempt 1 failed and attempt 2 cancelled: still failed;
    - attempt 1 failed and attempt 2 succeeded: passed;
    - two runs in both finishing orders;
    - a pending run;
    - a truncated run list;
    - a capped run list where every page loads.
  - Each case asserts the records, the combined result, and replay equality with the
    fake forge stopped.
- **Depends on:** M1, T2, T4.

#### T22. Publisher repository, trusted run clones, and object import

This is the trusted clone and publisher foundation, moved out of T14 so that T7 can use
it.

- **Builds:**
  - the bare publisher repository per publication repository, and kwoon's git
    environment (D4, steps 1 and 3);
  - independent run clones with `--no-local` and the `kwoon/work` branch (D4, step 2);
  - the frozen snapshot, the final-head read, the confined packing, and the import into
    the publisher (D4, steps 4 to 7);
  - the ancestry checks (D4, step 8).

  It makes no push and opens no pull request; those are T14.
- **Acceptance rows:**
  - PUB-G1b [neither the clone's hook nor its configuration works; no code from the
    clone sees a credential], for the clone-and-import half. T14 adds the push half;
  - PUB-G2G20 [the start head is not an ancestor of the final head: nothing is
    published, `failed`, and a question; ancestry unreadable: `unknown`], for the check
    itself. T14 asserts that no external write follows.
- **Check first:** [plan] git inside the confined sandbox packs from an alternate that
  has no `info/alternates`, and `index-pack --strict` rejects a corrupt object planted
  in the snapshot. If either fails, the implementer reports it on the task issue before
  building around it.
- **Verification:**
  - Deterministic, `cargo test --test import`. The fake harness plants each of these in
    its clone, one per case:
    - `objects/info/alternates` as a symlink to a canary secret file, and as a regular
      file naming the host's state directory;
    - an object file that is a symlink to the canary;
    - a FIFO and a device node under `objects/`;
    - a hard link to the canary under `objects/`;
    - `refs/heads/kwoon/work` as a symlink, and as a symbolic ref;
    - a `reftable/` directory;
    - a corrupt loose object;
    - a symlink or special file at `objects/pack/pack-<40 hex>.rev`.

    Each is rejected. The canary's bytes appear in no pack, no captured stdout or
    stderr, no record, and no question.
  - Test symlink substitutions at `.git`, `objects`, an object fan-out directory,
    `refs/heads`, the run-home root, a log directory, and the log itself. The host-side
    fixture supplies canary targets outside the permitted roots. Extraction must reject
    each substitution without opening the canary, publishing, or including its bytes in
    output, diagnostics, records, or questions. Keep the existing leaf-file,
    special-file, alternates, and hard-link cases. (T22 covers the clone cases; T1
    covers the run-home, log directory, and log cases.)
  - Include a hash-valid, structurally invalid reachable commit, such as one missing its
    author header. Assert that confined packing succeeds and the production import fails
    specifically at `index-pack --strict`. In a disposable control repository, the same
    pack must be accepted by `index-pack` without `--strict`; otherwise the fixture does
    not prove strict validation. No publishing action may follow the failed production
    import.
  - Independence: the fake harness overwrites an object file and appends to a pack in
    its clone. The publisher repository's files keep their SHA-256 values.
  - A clean run created by the specified `--no-local` command is imported successfully,
    including a fixture large enough to retain a pack and its default `.rev` file. The
    publisher passes `git fsck`, and the run and publisher share no object inode.
- **Depends on:** M1, T1, T4.

#### T7. Review runs, verdicts, and findings — blocked by #18 (OQ3)

- **Builds:**
  - `kwoon run --role review --pr <n>`: a run clone at the pull request head (T22),
    started in the wrapper with a private run home (T1);
  - the lock scope of D1 for runs: lock to record the launch, no lock while the agent
    works, lock again for the result;
  - the `run` record with launcher-stamped identity: harness, session, model and effort
    requested, provider account as an opaque id, family from the policy;
  - the observed model as #18 decides: the per-turn models from the harness's log when
    the launch-time check shows the run cannot write it, otherwise the launch record;
  - kwoon's own capture of the output;
  - a `verdict` record parsed from one final JSON block that kwoon asks for. An output
    that cannot be parsed gives `unknown`;
  - `finding` records. The prompt lists earlier findings with their ids, so the review
    can report `persists` or `fixed` against an id. kwoon validates every id it is given.
- **Acceptance rows:** G9 [a review whose model is unknown is rejected]. kwoon records
  `unknown` and never infers a model from the run's output. The gate side of G9 is in
  T10.
- **Check first:**
  - The spec revision in #18 has merged.
  - How each harness reports fallback events [to verify in T7].
  - Still to verify 3, in part: which fields the harness's log reports for the observed
    model and the session id, per harness, and whether kwoon can set the session id at
    launch.
- **Verification:**
  - Deterministic, `cargo test --test review_run`, with the fake harness:
    - a valid verdict;
    - an unparseable verdict gives `unknown`;
    - a verdict that names a finding id kwoon never issued is rejected;
    - a run whose output claims another model has no effect on the observed model;
    - a log without a model field gives `unknown`;
    - **Log rewrite (OQ3).** Use two separate fixtures. For `cross-model`, the author
      and review both have model M1, while the review log claims M2. For
      `fresh-session`, the review resumes the author's session S1, or its session
      identity is unestablished under the approved OQ3 boundary, while the log claims
      S2. The forged field must not establish the missing independence in either
      fixture. T10 checks the corresponding gate rejection. Include an untampered,
      independently established fresh session as a positive control. The maintainer's
      OQ3 decision determines how identity is established; these tests do not choose
      that mechanism.
    - **lock scope.** While a review run blocks on a signal from the test, `kwoon poll`
      and `kwoon answer` both complete within 30 seconds.
    - **Model identity (#18).** Each case uses its own fixture:
      - the session log is writable by the run: identity comes from the launch record,
        a log claiming another model can only downgrade it to `unknown`, and the `run`
        record says which evidence applied;
      - the session log is not writable: the per-turn models from the log are recorded;
      - one turn is served by a different model, such as a safety fallback: T7 records
        the per-turn models and fallback without folding the fallback into the
        requested value. T10 verifies that a policy naming the requested model is not
        satisfied. T15 verifies that the fallback is shown in the handoff, using a
        separate fixture whose policy permits every observed model so that handoff can
        occur;
      - the launch-time check cannot be run: the log is treated as writable.
  - Live, run by the operator: one review run per supported harness on the test
    repository.
- **Depends on:** M1, T1, T4, T8, T22; the spec revision in #18.

#### T8. Approvals, answers, questions, and the ruling archive

- **Builds:**
  - `kwoon approve <pr> --head <commit> --covers <what>`;
  - `kwoon answer <question> <choice>`;
  - question deadlines and lapse, derived from later records that change a fact the
    question depends on;
  - re-raising a question past its deadline;
  - the ruling archive of D3, with its durability check and crash recovery;
  - "an answer acts only through new records".
- **Acceptance rows:**
  - G21 [a question's pull request gets a new head: the question lapses, and an answer
    to it has no effect; a question past its deadline is raised again];
  - G8 [an `answer` dismisses a blocking finding and an `approval` covers a
    policy-layer path; when the local archive commit fails, neither takes effect]. The
    gate's decision on this is asserted in T10;
  - G8 [killed after the archive commit, before the `archive` record: after restart,
    the existing commit is recorded and nothing is committed twice].
- **Check first:** [plan] `core.fsync` and `core.fsyncMethod` exist and are read back as
  set in the git versions kwoon supports.
- **Verification:**
  - Deterministic, `cargo test --test rulings`:
    - an archive commit made to fail (read-only clone): no `archive` record, and a
      question is raised;
    - the git kill seam (D6) kills kwoon after `update-ref` moves `kwoon-checkpoints`.
      With the three kill assertions, then after restart one commit and one `archive`;
    - Independently remove reference hardening, remove object hardening, and change
      `core.fsyncMethod` to `writeout-only` in the effective configuration. Each case
      must prevent a successful `archive` record and the ruling's effect. Change the
      configuration at the scope actually consumed by the command; changing an
      overridden global value is not a negative control.
    - recovery finds a commit whose blob does not match: refused, `unknown`, and a
      question;
    - two commits naming the same source record: `unknown`, and a question;
    - an answer to a lapsed question is refused.
- **Depends on:** M1, T4, T5.

#### T9. Capability report

- **Builds:**
  - `capability` records for every item `kwoon doctor` tests, each with its test and
    "valid until";
  - start refusal when a floor item fails;
  - per-repository scopes, including credential routing (D6);
  - `not provided` for every guarantee not built yet (spec, "Delivery order"), and for
    G22;
  - the trust-model items: whether `kwoon approve` is reachable from a process kwoon did
    not start, and whether the account has root-equivalent rights (`docker` group,
    passwordless `sudo`);
  - G4 reported as degraded, citing T3's raw-writer test;
  - `released-binary` (D2);
  - `store-consistent` (D1);
  - whether the process runs in the isolated verification context (D6), and its fixture
    allowlist.
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
    time out, and to be skipped. Each gives `unknown` and, for a floor item, exit code 1.
  - The family test uses host settings with one family removed.
  - The root-equivalent test runs with a fake `id` and `sudo -n` on `PATH`.
  - A normal governed repository with a development build: `released-binary` is
    `not provided`, and every mode refuses to start for it. In the isolated
    verification context the same build runs, and `released-binary` is still reported
    `not provided`.
- **Depends on:** M1, T1, T4.

#### T10. Gate evaluator (shadow) and replay — blocked by OQ3 (through T7)

- **Builds:**
  - the nine gate steps in `crates/kwoon-gate`, writing the `gate` record (D1 dedupe
    rule), with the mode and the record that set it;
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
    for byte with the fake forge stopped.
  - The gate half of T7's log-rewrite test: that review is not counted.
  - The gate half of T7's fallback test: a run with one turn served by a fallback model
    does not satisfy a policy naming the requested model.
  - The rebuild test deletes `derived.db` between two passes and compares every
    decision.
- **Depends on:** M1, T6, T7, T8, T9.

#### T11. Release build and install

- **Builds:**
  - a release workflow, triggered by a version tag, that builds the two musl targets
    and `SHA256SUMS`. The task issue must say explicitly that it may change
    `.github/workflows/`;
  - the evaluator version, and the `released-binary` check against the release's
    `SHA256SUMS` (D2);
  - install instructions in `README.md`.
- **Acceptance rows:** G3 [a capability whose test did not run is `unknown`], for the
  `released-binary` item: an unreadable release gives `unknown`. It also provides the
  spec's "released version installed on the kwoon host" ("Gate and handoff"), which
  every mode on a real governed repository requires.
- **Check first:** none.
- **Verification:**
  - A tag on a test fork produces the binaries and `SHA256SUMS`.
  - `sha256sum -c` passes.
  - `kwoon doctor` on the downloaded binary reports `released-binary` `verified`.
  - A local `cargo build` reports it `not provided`.
  - The same release binary placed inside a governed clone reports it `not provided`,
    with the path as the reason.
  - An unreachable release URL gives `unknown`.
- **Depends on:** M1, T9.

#### T12. Shadow operation on kwoon's own repository — blocked by OQ7 and OQ3

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
- **Check first:** the maintainer's decision on OQ7. Checkpoints carry record bodies.
- **Verification:**
  - Live: at least one real pull request on kwoon's repository gets a `gate` record, and
    `kwoon replay` reproduces it.
  - The measurements are posted on #4. If records per month exceed 100,000, or a rebuild
    takes over 60 seconds, the implementer reports it before slice 2 starts.
  - Step 2 fails for agent runs started outside kwoon unless the operator covers
    authorship with an approval. That is expected until T14.
- **Depends on:** M1, T10, T11; the decisions on OQ3 and OQ7.

---

### Slice 2: checkpoints, publishing, live handoff, and the audit

#### T13. Checkpoint push and remote verification — blocked by OQ3

- **Builds:**
  - checkpoint commits and pushes (D3), including the first checkpoint, the cadence rule,
    and the "Push a checkpoint" external action with recovery;
  - `kwoon verify` against the remote branch;
  - the documented procedure for restoring the store from the checkpoint branch, which
    D1 points to.
- **Acceptance rows:**
  - G6 [an edited old record is found by comparison with the last pushed checkpoint];
  - G7 [push a checkpoint: killed after the remote accepted; after restart, no second
    checkpoint];
  - G8 [when the remote cannot be reached but the local archive succeeds, the gate
    passes, and the next checkpoint push carries both rulings].
- **Check first:** the policy repository's credential (D6).
- **Verification:**
  - Deterministic, `cargo test --test checkpoint`:
    - the git kill seam (D6) kills kwoon after a successful push to a local bare remote,
      with the three kill assertions;
    - an unreachable remote (a bad URL) leaves the rulings in effect, and the next pass
      pushes them;
    - the edit test changes `seq` 5 in the store, recomputes the chain, and runs
      `kwoon verify`, which reports `seq` 5;
    - Test both an absent local branch and a local branch containing an archived ruling
      before the first remote push. The former creates a parentless checkpoint; the
      latter extends the ruling history. In both cases the remote receives records from
      `seq` 1. In the latter case the ruling file and archive commit remain reachable. A
      competing remote branch creation causes failure without overwrite.
    - a remote branch for another store id: nothing pushed, and a question.
  - **Cadence, with a fake clock.** The binary reads the real clock. The test moves time
    by seeding the store with records whose `at` values are set relative to the test's
    start; seeding writes canonical, hash-chained records through kwoon's own library.
    No clock code is changed in the binary. Sequence:
    1. The last confirmed push is 2 minutes old, and one record was appended after it.
       Idle polls, which append nothing, push nothing.
    2. The same store, with the last confirmed push 11 minutes old: the next idle poll
       pushes.
    3. No unpushed records and a push 11 minutes old: an idle poll pushes nothing.
- **Depends on:** M1, T8, T10.

#### T14. Publishing an authoring run's work — blocked by OQ3 (through T7)

- **Builds:**
  - `kwoon run --role author`, with the publication and governed identities fixed at
    launch from host settings, and the lock scope of D1;
  - the push of D4 (step 9), on the import of T22;
  - the "Push the run's commits" and "Open the draft pull request" actions with their
    recovery rules. The recovery search uses the pull request list endpoint for all
    states in the recorded governed repository, without head or base filters, and
    compares all four endpoint identities after finding the marker (D6, “Complete
    reads”), never the search API;
  - `push` records, including observed pushes with author `unknown`;
  - the capability re-check before each publishing action.
- **Acceptance rows:**
  - PUB-G1a, PUB-G1b (push half), PUB-G2, PUB-G2G20 (no external write), PUB-G7a,
    PUB-G7b, PUB-G7c, PUB-G7d, PUB-G5;
  - G7 [push a run's commits; open a draft pull request];
  - G19 [a capability a publishing action needs lapses before that action: no external
    write].
- **Check first:** Still to verify 8: whether the token needs an extra permission to push
  changes to `.github/workflows/`, and how GitHub refuses such a push, so that kwoon
  records it as `failed` and raises a question.
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
  - Kills: the git kill seam after the push; the fake forge after pull-request creation.
    Each kill test makes the three assertions.
  - Recovery of “Open the draft pull request”, with an existing intent and no recorded
    outcome:

    - A marker match has a different endpoint identity: the intent is `unknown`, a
      question is raised, and no replacement is opened. Cover a retargeted base and a
      changed head branch or head-repository identity.
    - A complete read contains no intent-marker match: kwoon opens the pull request
      once. Cover both an empty list and a nonempty list of unrelated pull requests.
    - Several pull requests carry the intent marker: the intent is `unknown`, a question
      is raised, and no replacement is opened.
    - The search is unreadable or incomplete, including a failed page, a truncated read,
      or an unestablished completion rule: the intent is `unknown`, a question is
      raised, and no pull request is opened.
  - Live: PUB-G1a, PUB-G7a to d, and PUB-G5 on the test repository, with kills through
    the live kill seam.
- **Depends on:** M1, T5, T7, T9, T22.

#### T15. Live handoff and re-evaluation — blocked by OQ3

- **Builds:**
  - for a passing gate: the `gate` record, the intent, mark ready, request reviewers,
    the optional handoff comment, and the `handoff` record;
  - re-evaluation of handed-off pull requests on every pass, and back to draft on
    failure, except for step 8;
  - the per-reviewer intents and the history-based recovery of "External actions";
  - the hidden-marker comment search on the comment list endpoint;
  - the capability re-check before handoff.
- **Acceptance rows:**
  - G7 [mark ready; request reviewers, including every reviewer-request case in the G7
    row; post the handoff comment];
  - G10 [a handed-off pull request goes back to draft after a new commit], deterministic
    and live;
  - G19 [a capability lapses between evaluation and handoff: the handoff is refused].
- **Check first:**
  - Still to verify 5: the review-request history can be read completely, with event
    identity, and requests stay identifiable after a review or removal. This covers
    pagination, a cap, timestamp ambiguity, and unavailable history.
  - T2's results for the fork and outside-collaborator settings.
- **Verification:**
  - Deterministic, `cargo test --test handoff`. Kills after each forge write.
    Reviewer-history fixtures:
    - a reviewer who reviewed before the restart;
    - a removed request;
    - a host clock and a forge clock that differ;
    - a missing history page;
    - a capped history where every page loads;
    - a crash after a per-reviewer intent but before submission.
  - Live: the same on the test repository, through the live kill seam.
  - The handoff half of T7's fallback test: with a separate fixture whose policy permits
    every observed model, the handoff happens and shows the fallback.
- **Depends on:** M1, T10, T13, T14.

#### T16. Handoff audit — blocked by OQ3

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
- **Depends on:** M1, T15.

---

### Slice 3: post-merge tracking

#### T17. Merge observation and acceptance tracking — blocked by OQ3

- **Builds:**
  - `merge` records;
  - `acceptance` records from combined results, using the rule of T6 and the persisted
    observed run set;
  - superseding records for a late failure, and for a late `unknown`;
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
  - Q1 case 3, and Q4 cases 1 and 2.
- **Check first:** none beyond T6's.
- **Verification:** deterministic, `cargo test --test acceptance`. Every finishing order
  of the multi-run fixtures, and replay equality.
- **Depends on:** M1, T10, T16.

#### T18. Sync circuit breaker — blocked by #16 and OQ3

- **Builds:**
  - the red count of Q2;
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
    and turns the breaker on]. This applies with "distinct red" meaning a distinct
    target commit, once the spec change merges;
  - Q2 cases 1 to 4.
- **Check first:** the consistent breaker-unit spec revision defined in Q2 (#16) has merged.
- **Verification:** deterministic, `cargo test --test breaker`. A missing completion time
  makes the count `unknown`, and the gate fails at step 3 with a question.
- **Depends on:** M1, T17; the consistent breaker-unit spec revision defined in Q2 (#16).

#### T19. Acceptance recovery (G13) — blocked by OQ3

- **Builds:**
  - per-repository capabilities for re-running the target's CI and for the manual
    trigger;
  - "Re-run a cancelled acceptance run" and "Start a missing acceptance run", with their
    recovery rules and the complete-read rules of D6;
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
    accepted. A truncated or capped run list makes the start intent `unknown`.
  - Live on the test repository, with a `workflow_dispatch` acceptance workflow and one
    without it.
- **Depends on:** M1, T17.

---

### Slice 4: run settings and readiness for live

#### T20. Run settings — blocked by OQ3 (through T7)

- **Builds:**
  - per-run model, reasoning effort, and context-compaction window through each
    harness's options, the seeded settings in the private run home, or environment
    variables in the wrapper;
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
- **Depends on:** M1, T7.

#### T21. Live negative suite and readiness for live mode — blocked by OQ3, OQ7, and #16

- **Builds:**
  - `tests/live/`: every row of kind "Live", or "Deterministic and live", run against
    the dedicated test repository through the release binary, in the isolated
    verification context;
  - `kwoon doctor` floor checks for live mode;
  - the mode-switch state machine (D6, "Mode switch"), including the promotion
    capability;
  - a report of the shadow promotion criterion.
- **Acceptance rows:** the live half of G5, G7, G10, PUB-G1a, PUB-G7a to d, and PUB-G5;
  G11; G13. Plus the spec's "Moving from shadow to live" criteria.
- **Check first:** none.
- **Verification:**
  - The operator runs `cargo test --test live -- --ignored` with the release binary and
    posts the result on #4.
  - Run the mode-switch scenarios against the fake forge in the isolated verification
    context, using its scoped test CA and the unchanged production mode-switch state
    machine.
  - Test repeated pending polls, withdrawal before answering, expired and superseded
    questions, a second live request after returning to shadow, insufficient matches,
    fewer than three passes, repeated evaluations of one pull request, and missing or
    failed live-negative evidence. None may activate live. One qualified request and its
    valid answer activate live once; replay reconstructs the same effective mode and
    authorization.
  - Negative controls of the isolated verification context (D6): test observations
    used toward real promotion are refused, and live cannot be activated without a valid
    confirmation.
  - Switching kwoon's own repository to live mode stays the operator's decision.
- **Depends on:** M1, T12, T15, T16, T18, T19, T20.

## Coverage of the guarantees

| ID | Level in the MVP | Tasks |
|---|---|---|
| G1 | Degradable; provided for runs kwoon starts | T1, T14, T22 |
| G2 | Floor | T9, T10, T14, T22 |
| G3 | Floor | T9, T11 |
| G4 | Degraded | T1, T3, T9 |
| G5 | Floor | T3, T5, T10, T12, T14, T16 |
| G6 | Degraded to the last pushed checkpoint | T3, T13 |
| G7 | Floor | T2, T5, T13, T14, T15, T19 |
| G8 | Floor | T4, T8, T9, T10, T13 |
| G9 | Degradable along the fallback chain | T7, T10, T20 |
| G10 | Floor (never carried over) | T10, T15 |
| G11 | Degraded (kwoon does not merge) | T15, T16 |
| G12 | Tunable | T10, T18 |
| G13 | Floor where rights exist, degradable elsewhere | T17, T19 |
| G14 | Tunable | T6, T17, T18 |
| G15 | Tunable | T7, T10 |
| G16 | Floor (policy layer), tunable (others) | T8, T10 |
| G17 | Floor | T4 |
| G18 | Floor | T3 |
| G19 | Floor | T9, T14, T15 |
| G20 | Floor | T6, T10, T17, T22 |
| G21 | Tunable | T5, T8 |
| G22 | Not provided in the MVP, as the spec states | T9 reports it `not provided` |

Every acceptance row of both spec tables is assigned to a task above. The live halves are
also run together in T21.

## Decisions on the open questions (reviews of `24a470c` and `b673730`)

Decided on #15 on the maintainer's behalf; the maintainer can overturn them at merge. A
decision that fills a gap the spec leaves open stands in this plan: Q1, Q3, Q4, OQ5, and
OQ6. A decision that changes spec text goes through a spec pull request: Q2 and OQ10,
both carried by #16. Tasks that depend on them stay blocked until #16 merges.

| Item | Decision | Where it is applied |
|---|---|---|
| Q1 / OQ2: pending runs | Adopted; the observed run set is persisted | Q1; T6, T17 |
| Q2 / OQ1: breaker unit | Failed target commits; the consistent breaker-unit spec revision defined in Q2 (#16) first | Q2; T18 blocked |
| Q3 | Confirmed | Q3; T10 |
| Q4 | Adopted, with the anchor as the start when nothing passed | Q4; T17 |
| OQ4: red-fix override | Applies only to a red target | Q3; T10 |
| OQ5: mode switch | No new record kind; every `gate` record names its mode; the switch to live is confirmed by an `answer` | D6, "Mode switch"; T21 |
| OQ6: one writer | One authoritative writer at a time, with bounded lock scopes | D1; T3, T7 |
| OQ8: platforms | Linux only | D2 |
| OQ9: CI platform | T1 must show a runner that works; a denied probe fails | T1 |
| OQ10: test execution | The `test = true` flag is removed; an isolated verification context replaces it; spec revision in #16 first | D2, D6; T2, T4, T5, T21; blocked tasks |

## Open questions

These are places where the spec is silent, or where this plan reads or tightens it. The
plan does not change the spec; the maintainer decides each one.

- **OQ3. The harness log is writable by the run (decided by #18).** The log counts only
  when a launch-time check shows the run cannot write it; otherwise identity falls back
  to the launch record. The observed model is a per-turn set, and fallbacks are
  recorded. T7 and its dependents stay blocked until #18's spec revision merges (see
  "Blocked tasks").
- **OQ7. Where the policy repository lives (held for the maintainer).** Checkpoints carry
  every record body, including pull request numbers, task names, and opaque account ids.
  kwoon's repository is public. Recommendation from the review: a separate private
  repository for the pilot. The location is a deployment choice that the spec makes
  configurable, and D6's credential design covers either location. To be decided before
  T12, the only task it blocks, and through T12, T21 (see "Blocked tasks").

## Proposed, not in this plan

- Landlock as a second wrapper for hosts that deny user namespaces (D5, option 3).
- A separate network namespace for runs, with a forwarding proxy limited to the
  provider's API (D5, "Network").
- `cargo deny` or a similar check on dependencies in CI.
- Signed releases.

Each would need its own issue.
