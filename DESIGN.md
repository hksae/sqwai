# sqwai — Design

Status: this document describes the design, not the state of the build. It says
what the host owns and what each mechanism guarantees. Decisions, their history
and their refusals live in `Changes.md`; behavior lives in the code. When code
and this document disagree, the document is wrong until deliberately changed,
and the fix happens in the same commit as the code.

Reading order: §0 → §1 → §2, then reference.

---

## 0. Thesis

sqwai is a terminal coding agent whose difference is **memory and visibility**.
The host keeps what the model cannot hold — the goal, the plan, durable facts,
pre-images of every write — and shows what the model cannot hide — the journal,
diffs, exit codes, timings.

Rigidity survives only where irreversibility does. A mechanism that constrains
without obstructing is a skeleton; one that obstructs to prove a point is a
prison, and prisons make agents that game rather than agents that are honest.
So the standing test for any gate: *does it prevent something the user cannot
recover from, or does it prevent a mistake the model could simply be told
about?* Only the first earns a refusal.

## 1. Principles

- **Code is the source of truth.** Documentation, memory and summaries are
  claims about code; the code decides.
- **Memory is files; an index over them is a cache.** Nothing durable lives
  only in a database, a graph, or context.
- **Bounded everything.** Output caps, byte caps, step budgets, timeouts,
  depth-2 walks. An unbounded read is a bug with extra tokens.
- **Degrade, don't refuse.** A missing symbol index falls back to text search;
  a narrow gate that cannot decide says so and lets the work through.
- **Prefix stability.** The stable prompt layers are byte-stable across a
  session; volatile content rides at the end. Per-turn churn re-keys the
  provider's cache and costs every request.
- **Deterministic first, model second.** Host-built facts before
  model-generated summaries; the model is asked to check, not to certify.
- **Prompt shapes forms, not traits.** Descriptions teach when to fire a tool;
  they do not install character.
- **Observations vs claims.** The host records what it observed; the model
  states what it concluded. The boundary is the trust model, and every block in
  this file is drawn along it.

## 2. The skeleton: what the host owns

All of it lives under `.sqwai/` in the project, which the file tools and the
shell classifier refuse to write directly: plan, journal, memory and checkpoints
are reachable only through their own tools. Inside `.sqwai/`, `skills/` and
`config.toml` are the two writable exceptions.

**Plan** — an event-sourced, host-owned task structure, one active plan per
session, versioned:

    Plan = version, id, status, goal, constraints, criteria, steps, budget, revision
    Step = id, title, status, started/finished, summary/reason, evidence, step_epoch, stale_goal

`goal` and `constraints` are the user's; the model may not change them
silently. `criteria` are the agent's own done-notes — plain text, at least one
required at `create`, appendable with `add_criteria`. The host runs none of
them and grades none: they exist so the target survives compaction, not so a
gate can fire. `steps` carry intent in their titles; `evidence` is a host index
of journal records attached to the step, written by attribution rather than by
the model.

Transitions are journalled first and applied second, so a crash between the
record and the file heals on replay: an orphaned create intent rebuilds the
plan, a torn plan file rebuilds from the create intent plus every later op, and
a rebuild that cannot match its journal quarantines the bytes instead of
guessing. Plans are re-opened per session by id; a session never inherits
another session's plan.

**Journal** — the append-only event log, one file per session, every line
addressable as `j#<seq>`: `user_msg`, `tool_call`, `tool_result`, `file_diff`,
`diagnostics`, `note`, `plan`, `checkpoint`, `provider_error`, `compaction`.
It is the observer's data source, not a mechanism under test: it records
failures and refusals too. Attribution (session, plan, step, step epoch) is
stamped by the host at write time, never supplied by the model.

**Memory** — two files. `MEMORY.md` holds durable project facts,
`USER.md` the user's own preferences and agreements; both are injected into the
stable prefix of every later session, which is why writing them is a
same-turn duty rather than a courtesy. `memory_write` applies immediately — no
approval dialog — under a section/scope schema, a `replaces` correction path, a
secret screen, a size limit, and provenance with a date. Recent history is
pulled, not pushed: `journal op=recap` summarizes the last days per session from
host records — plan and its status, files touched, call counts, open threads —
and nothing of it rides in the prefix unless it is durable memory.

**Checkpoints and undo** — pre-images of every write, so a session's damage is
reversible. Local shadow repository or a configured remote store; hash-gated,
so a re-write of identical bytes costs nothing. Undo reads the chain of records
rather than trusting any summary of it.

## 3. What the model sees, in order

One cached `stable_prefix`, assembled in this sequence:

    system.md              (built-in, overridable by config_dir/system.md)
    + AGENTS.md, .sqwai.md project rules, the latter winning on conflict
    + <process_environment> platform, shell, working directory — immutable per process
    + memory               USER.md, MEMORY.md — durable facts only
    + skills               the enabled skill bodies

Then, as its own cached part:

    <session_environment>  date, OS, git branch and recent commits, declared +
                           probed toolchains, directory tree — captured at
                           session start, rebuilt after compaction and on
                           session switch

Then, per request inside the turn: the plan's goal and constraints as a cached
part, the step-state tail as a volatile part, and the mode line as a volatile
part — after everything cached. `runtime_context` is deliberately empty: a
changing block ahead of the history defeats provider prefix caching for the
whole session. Facts that change either arrive in the plan tail (which changes
only when the plan does) or are one tool call away.

The volatile parts are assembled into one **host block** that trails the
history as a `system` item, inserted before the newest user turn so the last
thing in a request is always the human's words or a tool result. It used to ride
as a `user` item at the very end, on every iteration of a tool loop, and a live
session answered that block instead of the user: it re-verified a tree it had
already verified, five byte-identical times. Host state describing itself as a
user turn is the one layout mistake a chat model cannot be argued out of.

**Compaction** replaces the discarded head with a host-built anchor —
journal-derived facts: files this session changed, what was last verified, open
assumptions, decisions, recent failures — under the header
`ANCHOR (host-generated working memo; journal facts, not a summary)`. It rides
only where history has actually been dropped, never on every request: while it
did, `system.md` called it the source of truth "because earlier history may be
gone", which taught the model to distrust the conversation in front of it. The
model is never asked to summarize its own goal: a summary inherits the errors of
what it summarizes, and constraints are the first thing it drops. The plan is the
model's only plan source and is rebuilt live per request, so a turn-start
snapshot cannot outlive a mid-turn edit.

## 4. Tools

29 registered tools in families: read (`read`, `ls`, `glob`, `grep`,
`outline`), write (`write`, `edit`, `multi_edit`, `patch`), execute (`bash`,
`bash_output`, `bash_kill`, `sleep`), git (`git_status`, `git_diff`, `git_log`,
`git_show`, `git_commit`, `git_stage`, `git_branch`), web (`websearch`,
`webfetch`), delegate (`subagent`), durable state (`plan`, `note`, `journal`,
`memory_write`), and `ask_user`.

Every description is a behavioral trigger: it says when to fire, not only what
it does. A tool that needs no approval says so; a tool stopped by a seatbelt
explains the seatbelt and what to do instead. Two rules hold everywhere: the
schema in the request is identical in Plan and Act mode (modes are enforced at
dispatch, where the authority already lives), and nothing in a description
invites the model to ask permission the host does not require.

`subagent` children inherit the mode, cannot nest further, are read-only unless
a task declares `write:true` with `paths:[...]`, and are attributed to their
own fresh session — with the parent's external taint carried into the child's
gates, because a child starts with an empty journal.

## 5. Seatbelts: what still refuses, and why it survives §0

Each of these stops something a user cannot recover from by themselves.

- **Dangerous command shapes and exfiltration** — `rm -rf`, sudo, disk ops,
  force-push, sending data outward — are approved by a human, once per command,
  before execution. The classifier is deterministic and quote-aware; a blocked
  command never reaches a dialog.
- **Host-owned state** — `.sqwai/` — cannot be written by file tools or shell.
  Without this, "append-only" and "host-written" would be requests, not
  guarantees.
- **Writer-scope of a subagent** — a child declared for `paths:[...]` writes
  only inside it. The extraction is deterministic and counts only what a shape
  *writes*: a mover's destination (its sources are reads), a redirect
  destination, `tee` operands, `dd of=`, in-place `sed` files but not its
  script, `/dev/null` as a sink; canonical paths, so a symlink inside scope
  cannot point outside it. Writers outside the list are documented best-effort
  fail-open — refusing all shell for a scoped child would brick its legitimate
  test runs, and the journal plus the shadow copy see those writes anyway.
- **SSRF gate** — webfetch refuses metadata names and non-public address ranges
  always, and re-checks every redirect hop. Loopback is reachable only through
  an explicit user `[web].allow_hosts` entry, because a dev server on
  `localhost:3000` is legitimate and a bare `curl` already reached it silently
  through the shell. The section is not project-overridable: a cloned
  repository must not widen the hosts its own tooling may reach, and remote
  catalog text gets an empty list.
- **Tainted egress** — after the session has consumed external content
  (web, MCP), an egress-shaped command asks once per (session, kind). The
  answer is journaled as `user_ack` and read back from the journal, so it
  survives resume and restart; a headless context denies instead and never
  inherits a human's yes. N identical dialogs would be friction without
  safety, and a clean content split between "foreign" and "local" bytes is not
  honestly computable: a commit made after an injection may carry injected
  content.
- **Plan shape** — `create` needs a goal, at least one criterion, and steps.
  This is the only plan refusal left, and it checks the completeness of a form,
  not the quality of the work.
- **A wrong plan is abandoned, not silently rewritten** — `abandon` (free for
  both actors) requires a reason that goes in the journal; `block_plan`
  requires the quoted contradiction. Neither stops work; both make the drift
  visible, which is what the future reader and the user's own memory depend on.
- **Secrets** — screened out of memory writes, and never committed or pushed
  without an explicit request.

Dialogue discipline in the approval dialog is part of the mechanism: deny is
preselected, a click only chooses, Enter commits, and a grace window stops a
focus-steal from approving a destructive command.

## 6. Claims, and what the host makes visible

The model reports what tools showed and marks everything else as inference.
A check result binds to the state it checked and to what it checked — nothing
more. `note` holds decisions, assumptions, rejected approaches, lessons and
blockers; `journal` holds observations, and reading it before claiming anything
about the past is the difference between a memory and a guess. Unverified work
is reported as unverified.

This is a norm backed by visibility, not by a gate: the host cannot prove an
honest sentence, but it can make every claim checkable after the fact.

## 7. Known limits

Stated plainly, because a document that hides them makes worse contributors.

- A write made through `bash` is not always attributable to a step; attribution
  degrades rather than refusing.
- The secret scan sees file-tool diffs. Bytes written by shell, and numeric
  magic constants, pass silently.
- Verification proves only what it runs: in the Patchwork study, 65 of 67
  structural defects passed tests, typechecker and SAST simultaneously. An
  agent's self-check is a lower bound, not proof. The journal makes it visible,
  not omniscient.
- LSP diagnostics are trusted testimony: the server comes only from the user's
  global config and cannot be re-pointed by a project, but a compromised server
  can still lie about `errors: 0`.
- DNS names are not resolved by the SSRF gate: resolution races the connect, so
  a hostile-DNS rebinding residual remains — documented, not fixed.
- Provider refusals under aggressive content filtering are mitigated (fallback
  chain, effort levels, journalled refusals, user override) rather than solved.
- Implicit writes — a test run rebuilding `target/`, a `cd` game — escape the
  scope extractor by design; the cap on that ambition is stated in §5.

## 8. Excluded by policy

Not to be re-proposed, with the reasoning kept in `Changes.md` §13:

- Multi-agent orchestration beyond the current `subagent` model — it dissolves
  ownership of the plan.
- Embeddings or semantic search as a source of facts — nondeterminism where a
  fact is needed.
- Automatic prompt training / self-improvement — against "code is the source of
  truth".
- A web UI before a navigator proves useful (and the code graph is gone).
- Script plugins — extensibility is covered by MCP.

## 9. Working here

- `cargo check` for a fast compile; `cargo fmt --all --check`,
  `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test
  --locked` are the gates. Toolchain is pinned by `rust-toolchain.toml`; build
  the release binary only when asked.
- Commit completed work, including the smallest fix, once verified.
- TUI invariants: usable in a narrow terminal and with long lines; every
  rendered row respects available width; render caches are invalidated when
  content, layout or dimensions change; keyboard, mouse, focus, resize,
  scrolling and overlay behavior is preserved when a view changes.
- Sessions stay distinct: new, resumed and switched sessions are separate
  objects, and opening the application never persists an empty placeholder.
- Host-owned plan, evidence, checkpoint, memory, safety and untrusted-content
  behavior is preserved by any change to this design.
