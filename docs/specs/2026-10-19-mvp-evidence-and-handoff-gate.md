# MVP: evidence contract and handoff gate

- Status: draft
- Tracking issue: #4
- Drafted: 2026-10-19

## Summary

The MVP makes one promise: an agent's pull request leaves draft and reaches whoever
merges it only through kwoon, and only when kwoon holds evidence, bound to the exact
commits involved, that the change was checked and reviewed under the operator's policy.

kwoon does not merge in the MVP. It hands the pull request off: it marks it ready for
review and requests reviewers. The merger then merges by their own rules. The merger may
be the operator, an upstream maintainer, or a tech lead. kwoon then checks that what was
merged is what it handed off.

kwoon verifies all of this from its own records. It never relies on the forge to enforce
any of it. A forge's own protections (branch rules, required checks, separate bot
accounts) are optional hardening. kwoon uses them when they exist and reports them, but
every guarantee must hold without them, or be reported as degraded.

The first governed repository is kwoon itself, on GitHub. Merging by kwoon, merge queues,
scheduling, model routing, full sandboxing, other forges, and the learning loop come in
later specs.

## Why this comes first

- Every later part (scheduler, routing, learning loop) reads and writes the evidence
  records. A wrong scheduler can be replaced; a wrong record format means migrating the
  whole history. So the record format and its first consumer, the handoff gate, go first.
- This design learns from an earlier multi-agent setup. The failures that cost most
  there were of two kinds:
  - gates that trusted a claim instead of evidence, such as a review bound to a head that
    later changed, or a "passed" check that tested something else;
  - state kept in two places that drifted apart.

  The MVP addresses both.
- This is a hypothesis about where the value is, not a measured result. The MVP records
  enough to check it later (see "Measures").

## Who uses kwoon

kwoon targets individuals first: one machine, one operating-system user, often one forge
account. The person running kwoon (the **operator**) is not always the person who merges.
Three settings must work with the same design:

| Setting | Who merges | What kwoon can rely on |
|---|---|---|
| The operator's own repository | The operator | Full rights on the repository, but often one account shared with agents. |
| Contributing to someone else's project | An upstream maintainer | A fork and pull requests. No merge rights; the upstream CI and review rules are not the operator's. |
| A company repository run in personal mode | A tech lead | Push rights to branches, no merge rights; the company's plan and settings are not the operator's. |

So kwoon governs what the operator controls: what the operator's agents produce, and
when it is handed to the merger. Merging stays with the merger. Precedents for the same
split:

- GitHub Copilot cloud agent opens only draft pull requests. It cannot mark them ready
  for review, approve them, or merge them, and the person who asked for the work cannot
  approve it.
- Renovate merges its own pull requests only when it has the rights and the repository
  allows it. Otherwise it leaves them for people and assigns reviewers.

## Why kwoon does not rely on the forge

- **One account cannot tell people apart.** When agents act through the operator's forge
  account, the forge sees one user. It cannot tell an agent's approval from the
  operator's, or the author from the reviewer. Claude Code cloud sessions, for example,
  act on GitHub through the account the operator connected.
- **Enforcement features depend on the plan and on admin rights.** GitHub documents
  rulesets for its Team and Enterprise plans. A contributor or a company developer is
  usually not an admin of the repository they work on.
- **Some setups have no forge at all.** A bare git remote is enough for kwoon's personal
  tier.

So kwoon keeps its own change record: runs, pushes, reviews, checks, approvals,
handoffs, and the merges it observes. A forge adapter maps it onto a forge. Without a
forge, a local implementation does the same. The MVP builds only the GitHub adapter, but
no gate step depends on a GitHub enforcement feature.

## Scope

In the MVP:

1. **Evidence contract v1**: the record kinds, their fields, and who may write each.
2. **Event store**: one append-only, hash-chained store on the kwoon host.
3. **Handoff gate**: evaluates each draft pull request from kwoon's own records. When it
   passes, kwoon marks the pull request ready and requests reviewers. It keeps
   re-evaluating handed-off pull requests, and turns one back into a draft when it no
   longer passes.
4. **Approval channel**: the operator approves and answers through `kwoon approve` and
   `kwoon answer` on the kwoon host, a channel agents cannot reach.
5. **Handoff audit**: detects a pull request that left draft, or was merged, at a head
   kwoon did not hand off. It also detects a push after the handoff. Where the operator
   can push to the target branch, it also detects target-branch commits that no handoff
   accounts for.
6. **Policy v1**: a versioned file in a git repository the operator controls. It takes
   effect only after the operator accepts that commit on the host. Its options are
   closed.
7. **Run recorder**: starts agent runs, records who ran them from the launcher side, and
   keeps local runs away from kwoon's state and secrets.
8. **Post-merge tracking**: follows each merge of a handed-off pull request to a
   finished acceptance result. A red target branch pauses new handoffs.
9. **`kwoon doctor`**: the capability report. kwoon refuses to start when a floor
   guarantee cannot hold.

Not in the MVP (each needs its own spec):

- **`merge` mode**: kwoon merges itself when the operator has the rights and turns the
  mode on. Merge queues belong to that spec (see "Later: merge mode and merge queues").
- A scheduler that decides the next step for each piece of work.
- Seats and model routing, and pools of several accounts per harness.
- Full sandboxing of local agent runs (network filtering, per-role users). The MVP only
  hides kwoon's state and secrets from them.
- Forges other than GitHub, and the local implementation for repositories with no forge.
- Carrying a verdict over to a new head. The MVP never carries one over (see G10).
- Approving from another device. In the MVP, approvals happen at the kwoon host.
- A knowledge provider, and the learning loop.
- High availability. One kwoon host is the single authority.

## Terms

- **Operator**: the person who runs kwoon, owns its policy, and approves through it.
- **Merger**: whoever merges pull requests in the governed repository: the operator, an
  upstream maintainer, a tech lead, or the repository's own bot.
- **Governed repository**: a repository where kwoon hands off the operator's agents'
  pull requests. The operator may or may not own it.
- **Policy repository**: a git repository the operator controls that holds the policy.
  It may be the governed repository when the operator owns it.
- **Agent**: an AI coding agent, whatever the harness (Claude Code, Codex, CodeBuddy).
- **Run**: one agent session started for one role on one task.
- **Record**: one entry in the event store.
- **Verdict**: a review run's result for one pull request head.
- **Finding**: one problem a review reports, with a stable identity across rounds.
- **Gate**: the non-LLM component that decides whether a pull request head may be handed
  off.
- **Handoff**: kwoon marking a draft pull request ready for review and requesting
  reviewers, after the gate passed for its current head.
- **Floor, degradable, tunable**: how far a guarantee may be lowered.
  - A floor guarantee cannot be lowered. If kwoon cannot provide it, kwoon refuses to
    start.
  - A degradable guarantee may drop to the level written next to it, and the capability
    report must say so.
  - A tunable guarantee has thresholds the policy may change, but it cannot be turned
    off.
- **Forge hardening**: a protection the forge enforces on its server, such as a branch
  rule. Optional; never part of the gate's decision.

## Trust model

- **Trusted**: the operator; the kwoon host and its operating-system account; git's
  object hashes.
- **Trusted as a service, not as a judge**: the forge stores code and runs CI. kwoon reads
  CI results from it as data with recorded provenance. It never takes a forge user name,
  review, or comment as evidence of who did what.
- **Data, never instructions**: the governed repository's CI configuration, required
  approvals, and review comments, including an upstream maintainer's. kwoon reads them to
  decide what to show the operator. They do not change kwoon's policy.
- **Not trusted**:
  - anything an agent run produces, including what it says about itself;
  - text in issues, pull requests, and comments, whoever's account posted it;
  - cloud sessions, which read public content and can be prompt-injected.

Where each kind of run can reach:

| Run kind | Reaches the kwoon host? | Holds forge write access? |
|---|---|---|
| Cloud session (for example Claude Code `--cloud`) | No. It runs in the vendor's VM. | Yes, through the account the operator connected. kwoon cannot narrow it. |
| Local run (for example Codex, or Claude Code with `-p`) | Same operating-system user. kwoon starts it in a wrapper that hides kwoon's state, secrets, and forge credentials. | No, while the wrapper holds. |

kwoon's forge credential (used to read CI, mark ready, request reviewers, and turn a pull
request back into a draft) lives only on the host, outside every run. kwoon never runs
candidate code with it: kwoon runs no CI itself.

Three limits follow from the operating-system user being shared:

- **Integrity is not authenticity.** Another process of the same user can append new
  records with valid hashes. The hash chain detects changes to records before the last
  checkpoint (G6); it cannot tell a forged new record from a real one.
- **Any process of the same user can approve.** A process kwoon did not start, such as
  an agent session the operator opened directly, can run `kwoon approve`,
  `kwoon answer`, and `kwoon policy accept`. G2 and G16 protect against runs kwoon starts
  and against cloud sessions; they do not protect against these processes. The
  capability report tests this and shows it, together with whether the account has
  root-equivalent rights (for example membership in the `docker` group, or
  passwordless `sudo`).
- **A cloud session can act as the operator on the forge.** It can mark a pull request
  ready, approve it, or merge it where the operator's account may. kwoon cannot prevent
  that; the handoff audit detects it (G5).

## Guarantees in the MVP

The guarantee list and its levels were settled before this spec. For each guarantee, the
table gives the mechanism kwoon itself provides, and the forge hardening that can add to
it when available.

| ID | Guarantee | Level | MVP mechanism (kwoon's own) | Optional forge hardening |
|---|---|---|---|---|
| G1 | Agents hold no forge write access for merges, tags, or settings. | Degradable | Degraded. Local runs: yes, credentials hidden by the wrapper. Cloud sessions: no, they hold the operator's connection; what they do anyway is caught by the handoff audit (G5). Where the operator has no merge rights (contributors, company), agents have none either. | A branch rule in the governed repository that stops the operator's account from merging or pushing to the target branch. |
| G2 | Delegation does not escalate: an action is authorized by its original proposer, a message never authorizes, and the sender is stamped by kwoon. | Floor | Each privileged action records who asked for it and which record authorized it. Only the operator, through the host, authorizes an exception. Forge reviews and comments are never evidence. Protects against runs kwoon starts and cloud sessions, not other processes of the operator's own OS user (see "Trust model"). | None needed. |
| G3 | The capability report lists only verified items, item by item. | Floor | `kwoon doctor` runs a test for each item and keeps its result. | None needed. |
| G4 | Only kwoon writes the event store. | Degradable | Degraded. Runs cannot reach the store: a cloud session runs on another machine, and the wrapper hides it from local runs. Other processes of the same OS user can still write it. | None. |
| G5 | State is derived from facts, and each fact has one source. | Floor | Derived state is rebuilt from records. The handoff audit checks the forge's pull request states and merges against kwoon's handoff records. | None needed. |
| G6 | Tampering with history is detectable. | Degradable | Degraded: detectable up to the last checkpoint kwoon pushed to the policy repository. | A forge that refuses force pushes to the checkpoint branch keeps those checkpoints safe. |
| G7 | External actions are neither repeated nor lost across crashes. | Floor | Intent, action, and confirmation are recorded separately, with stable ids. Recovery is defined per operation (see "External actions"). | None needed. |
| G8 | Rulings and policy live in git and can be reviewed. | Floor | The policy is in the policy repository. Operator answers that change how the gate judges are written there as well (see "Policy v1"). | None needed. |
| G9 | Reviews meet the policy's independence and count; an author never reviews its own change. | Degradable (only along the policy's fallback chain) | Judged from launcher-stamped identities (session, harness, model, family, provider account), never from forge user names. | None needed. |
| G10 | A verdict carries over only when the system can vouch for the change. | Floor | Strictest form: never carried over. A new head needs a new review. | None needed. |
| G11 | Only a non-LLM party merges, and the gate is re-evaluated at merge time. | Degradable | Degraded. kwoon does not merge in the MVP. Only kwoon hands off. It re-evaluates handed-off pull requests on every poll and turns one back into a draft when the gate fails. A merge between a change of facts and the next poll is caught afterwards by the audit and post-merge tracking. | A forge rule that blocks merging when the head changed after approval. |
| G12 | A change at risk of conflicting with the target branch is tested on the merged result first. | Tunable | When files the pull request changed were also changed on the target branch since their merge base, the head must contain the target tip before handoff. Otherwise it is handed off without syncing, and post-merge tracking is the fallback. A circuit breaker switches to "always sync" when the target turns red too often. | A forge rule requiring branches to be up to date. |
| G13 | The acceptance run after each merge always finishes. | Floor where the operator's account can re-run the target branch's CI; degradable elsewhere | kwoon follows each handed-off merge to a finished acceptance run, and re-runs a cancelled or missing run. Where the operator's account cannot re-run it, kwoon raises a question instead, and the capability report shows G13 as "tracked, not guaranteed" for that repository. | None needed. |
| G14 | A red target branch pauses further merges, opens a fix issue, and measures time to green. | Tunable | New handoffs pause. Instead of opening a fix issue, kwoon raises a question to the operator that lists the merges the red run covers, and it records time to green. kwoon opens nothing in the governed repository. | None needed. |
| G15 | Review loops end: a round limit, and findings with stable identities. | Tunable | Round limit and finding records; automatic duplicate detection later. | None needed. |
| G16 | Sensitive paths always get a human: the policy layer as a floor, CI and credential paths as tunable. | Floor (policy layer), tunable (others) | A `kwoon approve` record for the exact head. Forge approvals do not count. Protects against runs kwoon starts and cloud sessions, not other processes of the operator's own OS user (see "Trust model"). | Code-owner review by a separate person. |
| G17 | A policy change is authorized by the previously approved policy; runs read only approved policy. | Floor | kwoon reads only a policy commit the operator accepted on the host. Before accepting, it checks the new commit against the current policy's floors. | None needed. |
| G18 | Every record names the policy version in effect. | Floor | The schema rejects a record without it. | None needed. |
| G19 | kwoon refuses to start when a needed capability is missing and no fallback is written; it re-checks capabilities before a privileged action; floors cannot be configured away. | Floor | `kwoon doctor` at start, and gate step 9 before each handoff. | None needed. |
| G20 | When the answer is unknown, ask a human; never guess. | Floor | An unreadable or unknown input fails the gate with its reason and raises a question. | None needed. |
| G21 | Questions to a human arrive, have a deadline, and lapse when the facts change. | Tunable | Questions live on the host (`kwoon questions`), each with a deadline and the facts it depends on. Answers count only through `kwoon answer`. | None needed. |
| G22 | Measurements are taken by non-LLM code, out of reach of whatever is being measured. | Degradable | Not provided; the MVP has no scoring. | None. |

Not guaranteed at all:

- That the code is correct. kwoon guarantees that the gates held, not that the
  requirement was understood.
- That the merger merges only what kwoon handed off. kwoon detects a mismatch and tells
  the operator; it cannot stop a merger.
- Preventing an action by anything holding the operator's forge account, when the forge
  offers no enforcement. kwoon detects it and turns the pull request back into a draft
  or raises a question; it cannot stop it.
- Atomic merges across forges or repositories.
- High availability.
- Protection when the kwoon host or its operating-system account is compromised.
- Egress protection for agent runs.

## Evidence contract v1

Every record has these fields:

| Field | Meaning |
|---|---|
| `id` | Unique, sortable by time. |
| `kind` | One of the kinds below. |
| `schema` | Schema version. |
| `at` | UTC time kwoon wrote it. |
| `writer` | Always `kwoon` in the MVP. |
| `origin` | Who started the chain that led here: `operator`, `kwoon`, or a run id. |
| `authorized_by` | For a privileged action: the record that authorized it (an `approval`, an `answer`, or a passing `gate`). |
| `policy` | Policy repository and accepted commit in effect. |
| `supersedes` | For a correction: the record it replaces. Otherwise empty. |
| `prev`, `hash` | The hash chain. |

Kinds:

| Kind | What it records | Key fields |
|---|---|---|
| `policy` | A policy commit the operator accepted. | policy repository; commit; previous accepted commit; result of the floor check |
| `run` | One agent run. | role; task; harness; session id; provider account (opaque id); model requested and observed; family; effort requested and observed; context-compaction window; start and end; outcome; log digest |
| `push` | One push to a pull request branch. | repository; pull request; commits added; the run that made it, or `unknown` |
| `verdict` | A review result. | review run; repository; pull request; head and base commits; result (`approve`, `request_changes`, `unknown`); finding ids |
| `finding` | One reported problem. | stable id; pull request; category; severity; status (`open`, `fixed`, `persists`, `dismissed` with a reason); first and last run that saw it |
| `check` | One CI result read from the forge. | check id from the policy; repository; workflow file and its revision; run id, job id, and attempt; triggering event; commit the result is attached to; commit actually tested, when the forge reports it; conclusion |
| `approval` | An operator approval. | repository; pull request; head; what it covers (policy-layer paths, sensitive paths, authorship of unlinked commits, adopting an unaccounted change, a fix for a red target); entered through `kwoon approve` |
| `gate` | One gate evaluation. | repository id; pull request; target branch; head; target tip evaluated; merge base; evaluator version; input record ids; decision (`pass`, `fail`, `unknown`) and the reason for each step |
| `handoff` | A handoff, or turning a pull request back into a draft. | repository; pull request; head; the `gate` record it relied on; direction (`ready`, `back-to-draft`) and why |
| `merge` | A merge kwoon observed on the forge. | repository; pull request; merge commit; head at merge; matching `handoff` record, or `none` |
| `audit` | One handoff audit. | repository; what was checked; result (`clean`, `unaccounted`, `rewritten`); the pull requests or commits that do not match |
| `acceptance` | Tracking of one merge to a finished acceptance result. | merge records covered; `check` records; outcome (`passed`, `failed`, `not finished`) |
| `capability` | One item of the capability report. | item; scope (host or repository); status (`verified`, `not provided`, `unknown`); the test that proved it; valid until |
| `question` | A question to the operator. | subject; the facts it depends on (repository, pull request, head, policy commit); deadline; answer; what made it lapse |
| `action` | An external action kwoon takes. | intent id; operation; target; state (`intended`, `done`, `failed`, `unknown`); external id |

Rules:

- **Identity comes from the launcher and the harness's own logs, never from the run's
  output, and never from the forge.**
  - kwoon records the harness, session, model, effort, and provider account when it
    starts a run.
  - Afterwards, it records the model and the actions it observes in the harness's log.
  - A field neither source gives is `unknown`, and an `unknown` never satisfies a policy
    requirement.
- **Identities are kept apart.** A forge principal, a provider account, a session, and
  the operator are different things. The independence rules below say which one each
  option compares.
- **Accounts are opaque ids.** The store and anything published hold no email address or
  user name of a model account.
- **Every pushed commit has a known author run or the operator's approval.** kwoon links
  each push on a pull request branch to the run that made it, from that run's log. A push
  it cannot link (for example, the operator's own, or a maintainer's suggestion commit)
  makes authorship `unknown` until the operator covers it with an `approval`.
- **A CI result counts only with its provenance.** A `check` record is written only when
  the job comes from the workflow file and job that the policy names for that check id,
  is attached to the exact head, and concluded `success`. `neutral`, `skipped`, `cancelled`, a
  job with the right name from another workflow, and a plain commit status do not count.
- **The store is append-only.** A correction is a new record that names the old one in
  `supersedes`.
- **A gate decision can be replayed.** The `gate` record names the evaluator version and
  every input record, so the same inputs give the same decision.

The plan fixes the canonical encoding, the hash algorithm, and the checkpoint format.

## Policy v1

The policy lives in the policy repository. kwoon reads it only from a commit the
operator accepted on the host with `kwoon policy accept <commit>`. There is one policy
in effect at a time, for every governed repository it lists.

Before kwoon records the acceptance, it checks the new commit against the current policy:

- every option has an allowed value;
- no floor guarantee is lowered;
- the policy-layer path list only grew.

If any check fails, the commit is refused and the current policy stays in effect.

A pull request in a governed repository can never change the policy in effect. When the
policy repository is the governed repository, the policy file is a policy-layer path. A
pull request that changes it needs an operator approval to be handed off. Even after it
is merged, the new policy takes effect only once the operator accepts its commit.

Options are closed. Each has a fixed set of values, and nothing in the file can lower a
floor guarantee.

| Option | Values | Notes |
|---|---|---|
| Governed repositories | Repository, target branch, the reviewers to request at handoff, and whether to post the handoff comment | Per repository. |
| Review independence | `fresh-session`, `cross-model`, or `cross-family`, plus an ordered fallback chain | Missing capability and no fallback: refuse to start (G19). |
| Review count | 1 or more | Counted as distinct review runs. |
| Round limit | 1 or more | Past the limit, the gate fails and asks the operator. |
| Blocking finding severity | A severity | An `open` or `persists` finding at or above it blocks the handoff. |
| Required checks | Check id → workflow file and job name in the governed repository | Read from the forge's CI on each pull request head. |
| Acceptance checks | Check id → workflow file and job name on the target branch | Required. Without one, kwoon refuses to start (G13). |
| Policy-layer paths | Paths | Always need an operator `approval`. The list always includes: the CI configuration (`.github/workflows/` on GitHub); when the policy repository is the governed repository, the policy file; and, in kwoon's own repository, kwoon's gate code. The list can only grow. |
| Sensitive paths | Paths, each `operator` or `cross-family` | `cross-family` entries are shown in the capability report. |
| Model families | Model id → family | Used to judge `cross-family`. An unlisted model is `unknown`. |
| Sync circuit breaker | Red count and window | Pilot value: 2 red target runs in 14 days. Once on, it stays on until the operator turns it off with `kwoon answer`. |

Answers that change how the gate judges are rulings. Examples: dismissing a finding, or
accepting a change kwoon did not hand off. Such an answer takes effect for the case at
hand. The operator writes any lasting change into the policy repository, as an accepted
policy commit (G8).

## Gate and handoff

The gate evaluates one draft pull request at its current head, against the current tip of
the target branch. It passes only when every step below passes. A step whose input it
cannot read gives `unknown`, and `unknown` fails with its reason (G20).

1. **Policy**: the accepted policy commit (G17).
2. **Authorship**: every commit between the merge base and the head is linked to an
   author run, or covered by an operator approval.
3. **Conflict risk (G12)**: kwoon compares two sets of files: those the pull request
   changed since the merge base, and those the target branch changed since the same
   base.
   - If the two sets share a file, or the circuit breaker is on, the head must contain
     the target tip.
   - Otherwise no sync is needed.
4. **Checks**: every required check has a `check` record that passed on this head.
5. **Reviews (G9, G10)**:
   - **Effective rule**: the strictest of the policy's independence rule and the rules
     of every sensitive path the diff touches.
   - **Count**: at least the required number of distinct review runs on this exact
     head whose latest verdict is `approve`, and no eligible review run whose latest
     verdict on this head is `request_changes`.
   - **Independence**: each counted review run meets the effective rule against every
     author run of the pull request:
     - `fresh-session`: a different session;
     - `cross-model`: also a different model;
     - `cross-family`: also a different family.

     A run never reviews a change it authored.
   - **Findings**: no finding at or above the policy's blocking severity is `open` or
     `persists`.
6. **Rounds (G15)**: a round is a distinct head that received at least one verdict. The
   number of rounds is within the limit.
7. **Human paths (G16)**: if the diff touches policy-layer paths, or sensitive paths
   marked `operator`, an operator `approval` covers this exact head.
8. **Target health (G14)**: the target is not red. It is red when the newest
   target-branch commit with a finished acceptance run failed. "Newest" is by commit
   order, not by when the run finished, so a late result for an older commit cannot
   hide a newer failure. An operator approval marking this pull request as the fix for
   the red target overrides this step.
9. **Capabilities (G19)**: every capability the policy relies on is still `verified` for
   this repository.

When the gate passes, kwoon hands off:

1. It writes the `gate` record and an `action` intent.
2. It marks the pull request ready for review and requests the reviewers the policy
   lists. If the policy says so, it also posts one comment that summarizes the gate
   record for the merger: checks, review runs with model and family, and approvals.
3. It writes the `handoff` record.

After a handoff, kwoon re-runs the gate on every poll. When a step fails, kwoon turns the
pull request back into a draft, writes a `handoff` record with direction
`back-to-draft`, and raises a question if the fix needs the operator. Step 8 is the
exception: a red target pauses new handoffs, but it does not pull back pull requests
already handed off. The red target was not caused by them.

**The gate that judges a change is never the code in that change.** It runs from a
released version installed on the kwoon host. A pull request that changes kwoon's own
gate is judged by the released gate, and the gate code is a policy-layer path in kwoon's
repository, so the operator must also approve it.

## Handoff audit

On every poll, for each governed repository, kwoon checks:

- **Pull request states**:
  - every pull request opened by the operator's forge account that is ready for review
    must have a `handoff` for its current head;
  - every merged pull request must have one for its head at merge.

  A pull request that left draft without a handoff is turned back into a draft. In both
  cases kwoon writes an `unaccounted` audit and raises a question.
- **Pushes after a handoff**: a new head on a handed-off pull request is a new head.
  The gate fails it at step 5, so it goes back to draft (see "Gate and handoff").
- **Target branch** (only where the operator's account can push to it): every new
  first-parent commit must be the merge of a handed-off pull request at its handed-off
  head, or covered by an operator approval. If the last audited tip is no longer in the
  history, the result is `rewritten`.

An `unaccounted` or `rewritten` result pauses new handoffs to that repository until the
operator answers with `kwoon answer`:

- adopt the change, recorded as an `approval`;
- or revert it, which kwoon reports but does not do itself.

The audit starts from an anchor the operator sets with `kwoon init` for each governed
repository.

## Approvals and answers

- The operator approves with `kwoon approve <pull-request> --head <commit> --covers
  <what>`, and answers questions with `kwoon answer`, both on the kwoon host.
- Cloud sessions cannot reach the host. Local runs are started in a wrapper that hides
  kwoon's state and sockets, so they cannot call these commands with effect.
- Other processes of the operator's operating-system user can call them (see "Trust
  model"). The capability report shows this.
- Approvals and answers given anywhere else do not count. This includes forge reviews
  and comments from the operator's own account, and an upstream maintainer's approval.
  kwoon shows them to the operator as information.

## External actions

Each external action has its own recovery rule, so a crash between doing it and recording
it neither repeats nor loses it (G7):

| Operation | Recovery after a crash |
|---|---|
| Mark ready, turn back into a draft | Read the pull request's state. The target state is idempotent: setting it again changes nothing. |
| Request reviewers | Read the requested reviewers; request only the missing ones. |
| Post the handoff comment | The comment carries the intent id in a hidden marker. Search the pull request's comments for it before posting again. |
| Push a checkpoint | Read the checkpoint branch; a checkpoint already there is not pushed again. |

An operation whose outcome kwoon cannot read is recorded as `unknown`. kwoon then raises a
question instead of retrying blindly.

## Runs

`kwoon run` starts one run and writes its `run` record.

- **Cloud session (Claude Code)**:
  - Started with `claude --cloud` in a pseudo-terminal; the CLI refuses to start a cloud
    session without one.
  - Follow-up messages go through `claude -p "<message>" --cloud <session-id>`.
  - kwoon reads the session's event log to observe the model and the actions it took.
  - The run is told to open its pull request as a draft and never to mark it ready. The
    audit catches it if it does.
- **Local run** (for example Codex, or Claude Code with `-p`):
  - It runs in a fresh clone of the head under work, inside a wrapper that hides kwoon's
    state, secrets, and forge credentials. Bubblewrap mounting an empty directory over
    them is one way.
  - The harness's own inner sandbox is turned off inside the wrapper, because nested
    sandboxes fail to start.
  - kwoon captures the output itself and records the verdict.
  - If the wrapper cannot start, `kwoon doctor` reports local runs as unavailable.

**Run settings**: for each run, kwoon sets the model, reasoning effort, and
context-compaction window where the harness allows it. It records both the requested and
the observed value. For Claude Code cloud sessions, the documented levers are:

- `/model <name>` and `/effort <level>` with an argument;
- settings committed in the repository's `.claude/settings.json`, or environment
  variables on the cloud environment;
- the compaction window, through `CLAUDE_CODE_AUTO_COMPACT_WINDOW` on the environment or
  `/autocompact <tokens>`. Cloud sessions set the compaction percentage themselves, so
  it cannot be changed.

It is still to verify which of these can be set per run, and which the session log
reports. A value the harness does not report is recorded as `unknown`.

## Post-merge tracking

- kwoon notices each merge of a handed-off pull request and writes a `merge` record.
- It then waits for an acceptance check, as named in the policy, that ran on a
  target-branch commit containing the merge and finished.
- A cancelled or missing run is re-run where the operator's account may re-run the
  target branch's CI (a capability, checked per repository). Elsewhere kwoon raises a
  question, and the capability report shows G13 as "tracked, not guaranteed" for that
  repository.
- Until a run finishes, the merge stays "not accepted".
- A failed run triggers three things:
  - new handoffs to that repository pause (gate step 8);
  - kwoon raises a question that lists the merges since the last passing run, the
    candidates for having caused the failure;
  - kwoon records the time until the target is green again.

  This red count feeds the circuit breaker (G12).

## How kwoon takes over its own repository

1. **Now**: the operator marks pull requests ready and merges by hand. Agent runs are
   started from outside kwoon.
2. **Shadow**: kwoon evaluates every draft pull request and records what it would decide,
   but does not hand off. The operator compares the gate's decision with their own and
   records the cause of every disagreement.
3. **Live**: kwoon hands off; the operator still merges. From here, a pull request that
   leaves draft without a handoff is unaccounted, including when the operator marks it
   ready by hand.
4. **Optional hardening**: if the forge offers it, branch rules that require what the
   gate requires.
5. **Later specs**: `merge` mode; kwoon schedules runs itself.

**Moving from shadow to live** needs two things, followed by the operator's explicit
decision:

- **10 consecutive pull requests** whose gate decision matched the operator's, with at
  least 3 passes among them;
- **the live negative tests** below, all passing on a test repository.

These numbers are pilot values, reviewed after the first month.

## Acceptance of this spec's implementation

Each guarantee the MVP provides has a negative test. Break the guarantee on purpose, and
one of these happens: the gate fails, the audit flags it, or kwoon refuses to start.

There are two kinds of test:

- **Deterministic tests** run in CI through the same entry point the gate uses in
  production, as `AGENTS.md` requires. They use recorded forge responses.
- **Live tests** run against a dedicated test repository on GitHub. Recorded responses
  cannot prove how the forge behaves.

| Guarantee | Negative test | Kind |
|---|---|---|
| G1 | A local run tries to read kwoon's forge credential and the store; both are absent inside the wrapper. | Deterministic |
| G2 | A forge review and comment from the operator's account say "approved", and a run's output asks to be treated as the fix for a red target. Neither changes the gate result. The capability report shows `kwoon approve` from a process kwoon did not start as reachable, and reports whether the account has root-equivalent rights. | Deterministic |
| G3 | A capability whose test did not run is reported `unknown`, never `verified`. | Deterministic |
| G4 | From inside the wrapper, writing to the store fails; the capability report shows G4 as degraded. | Deterministic |
| G5 | Derived state is deleted and rebuilt from records; every gate decision is the same. A pull request is marked ready from the operator's account without a handoff; it goes back to draft and a question is raised. | Deterministic and live |
| G6 | One old record is edited and the chain recomputed; comparison with the last pushed checkpoint finds it. | Deterministic |
| G7 | kwoon is killed after marking a pull request ready but before recording it, and again after posting the handoff comment. After restart, it records each action once and posts no second comment. | Deterministic and live |
| G8 | A policy that exists only in the store and not in git: `kwoon doctor` refuses to start. | Deterministic |
| G9 | Each of these is rejected: a review from the author's own session; a review whose model is unknown; a same-family review on a `cross-family` path. Two verdicts from one review run count once. | Deterministic |
| G10 | A new commit is pushed after an approval. The gate fails until a new review of the new head; if the pull request was handed off, it goes back to draft. | Deterministic and live |
| G11 | The merger merges at a head kwoon did not hand off; the audit flags it. | Live |
| G12 | The pull request and the target branch both changed file F since their merge base, and the head does not contain the target tip: the gate fails. Without a shared file, it passes without a sync. | Deterministic |
| G13 | The acceptance run of a handed-off merge is cancelled. kwoon re-runs it and keeps the merge "not accepted" meanwhile; without re-run rights, it raises a question. | Live |
| G14 | The target turns red: the next handoff is blocked and a question lists the candidate merges. A second red within the breaker window switches on "always sync". | Deterministic |
| G15 | The round limit is exceeded: the gate fails and asks the operator. | Deterministic |
| G16 | A pull request changes a policy-layer path with agent approvals only: the gate fails. | Deterministic |
| G17 | `kwoon policy accept` is given a commit that lowers a floor or removes a policy-layer path; it is refused. A merged policy change has no effect until accepted. | Deterministic |
| G18 | A record without a policy reference is rejected by the schema. | Deterministic |
| G19 | The only account of the required second family is removed, with no fallback written: `kwoon doctor` refuses to start. A capability lapses between evaluation and handoff: the handoff is refused. | Deterministic |
| G20 | A required check's result cannot be read: the gate reports `unknown`, fails, and raises a question. | Deterministic |
| G21 | A question's pull request gets a new head: the question lapses, and an answer to it has no effect. A question past its deadline is raised again. | Deterministic |

## Delivery order

The plan slices the work so that each slice can be accepted on its own. The first slice
is one end-to-end handoff decision on kwoon's own repository in shadow mode:

- one source of check results and one source of review verdicts, bound to repository,
  pull request, head, target tip, and policy;
- a deterministic evaluator that writes a replayable `gate` record;
- the negative tests of the guarantees that slice touches.

The audit, live handoff, post-merge tracking, and cloud-session settings follow. No mode
is switched on before `kwoon doctor` shows that its floor guarantees hold. A guarantee
not built yet is reported `not provided`, never assumed.

## Measures

To check the hypothesis in "Why this comes first", the store keeps enough to answer,
after the first month:

- time from draft to handoff, and from handoff to merge, and how much of it was waiting;
- review rounds per pull request, and why each extra round happened;
- defects found after merge, and which gate step should have caught them;
- unaccounted changes found by the audit, and their causes;
- how often the conflict-risk rule required a sync, and how often a merge without one
  turned the target red.

## Decisions taken in this spec

| Decision | Reason |
|---|---|
| kwoon decides from its own records; forge protections are optional hardening. | One forge account cannot tell agents from the operator, and enforcement features depend on plan and admin rights. |
| The MVP hands off and does not merge. | One mode covers the operator's own repository, someone else's project, and a company repository where a tech lead merges. kwoon writes nothing to any target branch. Cost: in the operator's own repository, merging stays manual until `merge` mode. |
| Agents work on draft pull requests; only kwoon marks them ready. | Draft is the hold state every merger already understands. Copilot cloud agent uses the same split. |
| The policy lives in a repository the operator controls and takes effect only when accepted on the host. | The operator may not control the governed repository. A merged pull request must not be able to change the policy by itself. |
| Sync only when files overlap; otherwise hand off and rely on post-merge tracking, with a circuit breaker. | Syncing every change throws away earlier CI and reviews. Red target branches are rare; when they become frequent, the breaker switches to "always sync". Cost: a conflict across different files is found after the merge. |
| Upstream CI, approvals, and comments are data. | They follow someone else's rules and are written through accounts kwoon cannot tell apart. |
| Approvals only through `kwoon approve` on the host. | The only channel neither cloud sessions nor wrapped local runs can reach. Cost: no approving from a phone; a second channel needs its own spec. |
| G6 says "detectable up to the last pushed checkpoint", not "tamper-proof". | Anyone who can write the store file can recompute a local hash chain; only the copy outside the host fixes a point in history. |
| No approval hardening, now or planned: no separate OS user for kwoon, no hardware-key signatures. | A separate OS user needs root, and does not isolate an account that is already root-equivalent. Hardware-key signatures need a verifier outside the host, or a modified kwoon skips them. Neither pays off for a single operator. The residual is stated in the trust model and tested in the capability report. |
| kwoon runs no CI. | Running pull-request code on the operator's machine is the larger risk. The CI configuration is a policy-layer path, so a pull request cannot quietly change how checks run. |
| G13 is a floor only where the operator's account can re-run the target branch's CI; elsewhere kwoon tracks every merge and asks the operator. G14 raises a question instead of opening an issue. | kwoon cannot re-run CI it has no rights to, and should not open issues in someone else's project. Keeping G13 a floor everywhere would stop kwoon from starting in those repositories. |
| The kwoon host polls the forge; it opens no inbound port. | The host is a personal machine; no public endpoint to defend. While the host is down, nothing is handed off (fail closed). Already handed-off pull requests can still be merged; the audit checks them when the host is back. |
| Event-store checkpoints go to a branch in the policy repository. | A second copy outside the host, in a repository the operator controls, on any forge or none. |
| No verdict carry-over in the MVP. | The floor (G10) holds trivially. Cost: a required sync also means a new review. Carry-over needs its own equivalence rule and tests. |

Left to the plan, after comparing how mature projects of the same kind chose: the
implementation language and packaging, and the event-store engine. The spec only
requires that the store be append-only, hash-chained, and written by one process.

## Later: merge mode and merge queues

The `merge` mode spec covers kwoon merging when the operator has the rights. Its merge
strategy includes a queue that tests changes on the latest target before merging:

- When the forge offers a native queue (GitHub merge queue), kwoon uses it.
- When it does not (GitLab without merge trains, or no forge), kwoon runs its own queue
  in the forge adapter.
- Precedents to compare first:
  - bors-ng, retired after GitHub shipped its native queue;
  - marge-bot, a serial rebase-test-merge bot with a batch mode that works on GitLab CE;
  - Zuul's speculative gating.
- Measure CI capacity first. A serial queue is only as fast as one full CI run per
  change.

## Open questions for the operator

None. Q1 (how G6 is stated) and Q2 (approving only at the host) were settled on
2026-10-20; see "Decisions taken in this spec".

## Still to verify (in the plan, before the matching task)

- Who may mark a pull request ready, or turn it back into a draft, in a repository the
  operator does not own: the author alone, or only accounts with write access. Also, on
  which plans and repository kinds draft pull requests are available.
- What a cloud session's GitHub connection can do (mark ready, approve, merge, change
  settings), with and without repository settings that deny those tools.
- Whether the operator's account can re-run CI in each kind of governed repository, and
  under what conditions upstream CI runs on a fork's pull request at all.
- How model, reasoning effort, and the compaction window can be set per cloud session,
  and which of them the session log reports.
- Which commit a pull-request workflow actually tests (the head, or a test merge with
  the base at that moment), and whether the forge reports it.
- Whether a follow-up message to an idle cloud session whose VM was reclaimed starts it
  again.
- That the local-run wrapper starts with each supported harness, and that the harness
  still reaches its own API inside it.
- Which owners and plans GitHub merge queue is available to (for the later `merge` mode
  spec).

## References

- GitHub Docs:
  - "About rulesets": availability by plan.
  - "Risks and mitigations for GitHub Copilot cloud agent": draft pull requests only; it
    cannot mark them ready, approve, or merge.
  - "About pull requests" and "Changing the stage of a pull request": draft pull
    requests.
- Renovate Docs, "Automerge": merging only with the rights; otherwise the pull request is
  left for people.
- Claude Code Docs, "Use Claude Code in the cloud": `--cloud`, follow-ups with `-p`,
  the GitHub proxy, `/model`, `/effort`, settings, and auto-compaction in cloud sessions.
- Zuul, config projects: changes to privileged configuration are not tested by their own
  new version, and they need human review. kwoon's rule that the released gate judges
  changes to the gate follows the same idea.
- git-appraise and Radicle: code review records stored in the git repository itself
  rather than in a forge's database. kwoon keeps its records outside git but writes
  checkpoints into it.
- in-toto and SLSA provenance: explicit subjects, producers, and materials for each
  piece of evidence. The `check` and `gate` records follow the same idea without adopting
  the framework in the MVP.
