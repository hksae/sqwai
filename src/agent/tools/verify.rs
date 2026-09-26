use super::Outcome;
use super::astgrep;
use super::ctx::ToolCtx;
use super::exec;
use super::policy::{acceptance_policy_hit, in_write_scope, lexical_clean};
use crate::agent::safety;
use crate::plan;
use serde_json::{Value, json};
use std::path::Path;

/// Longer than the tool default: an acceptance command is usually a test or
/// lint run, and cutting one off at two minutes would report a failure that is
/// really a timeout.
const ACCEPTANCE_TIMEOUT_SECS: u64 = 900;


/// How much of a baseline's failing output is kept inline (§12.12). Enough to
/// see *why* it failed — "cannot find function foo" is a baseline, "command
/// not found" is a typo — without copying a test log into the plan file.
const BASELINE_HEAD_CHARS: usize = 600;

/// How much of that reason is repeated in the create result. One line, short
/// enough to read in a collapsed tool row.
const BASELINE_REASON_CHARS: usize = 160;

/// Baselines captured at plan time, plus what to tell the model about the
/// items that did not get one.
pub(crate) struct BaselineProof {
    pub(crate) slots: Vec<Option<plan::Baseline>>,
    /// Rung 4, positional beside `slots`: the frozen outputs of `snapshot:`
    /// items, taken at the same moment under the same host-run rules.
    pub(crate) frozen: Vec<Option<plan::Snapshot>>,
    /// Rung 5, positional beside them: the frozen declaration shapes of
    /// `signatures:` items.
    pub(crate) shapes: Vec<Option<plan::ShapeFreeze>>,
    /// Frozen check inputs, positional: test/fixture hashes for every
    /// `cmd:`/`snapshot:`/`differential:` item. Frozen once, before any
    /// check runs — the only moment the pre-change tree is still current.
    pub(crate) inputs: Vec<Vec<plan::CheckInput>>,
    /// one line per item, prefixed with `\n` so they can be appended raw
    pub(crate) notes: Vec<String>,
}

/// Exit codes that mean the shell never ran the check at all: a typo or a
/// missing tool, not a failure of the code under test.
///
/// Best effort, and only where the shell actually says so. POSIX shells answer
/// 127 (not found) and 126 (not executable). `cmd.exe` and PowerShell answer 1
/// — the same code an ordinary failing check uses — so on Windows a typo *is*
/// recorded as a baseline. That is not a hole: `verify` needs the check to
/// pass, and a typo never passes, so the item simply never settles. The
/// portable guard is the failing output kept beside the baseline and shown to
/// whoever has to judge it.
pub(crate) fn check_never_started(exit: i32) -> bool {
    match crate::agent::shell::ShellKind::detect() {
        crate::agent::shell::ShellKind::Bash | crate::agent::shell::ShellKind::Sh => {
            exit == 126 || exit == 127
        }
        crate::agent::shell::ShellKind::Cmd | crate::agent::shell::ShellKind::PowerShell => false,
    }
}

/// Outcome of freezing one `snapshot:` item: kept, left unfrozen, or the
/// user stopped the turn mid-capture (later items must not start).
pub(crate) enum Frozen {
    Kept(plan::Snapshot),
    Empty,
    Cancelled,
}


/// Rung 4: freeze one `snapshot:` item's output at plan time. The same
/// host-run rules as a baseline — model-controlled text through the
/// classifier, typo guard, no raced runs — but any exit code freezes:
/// erroring the same way is behavior too. An empty output freezes nothing:
/// it discriminates nothing, the same way an already-passing check proves
/// nothing for `cmd:`.
pub(crate) fn freeze_snapshot(
    ctx: &mut ToolCtx,
    paths: &[String],
    index: usize,
    command: &str,
    notes: &mut Vec<String>,
) -> Frozen {
    // user's hard blocks and the exfil gate first; the classifier below
    // keeps its own refusal shape
    if let Some(hit) = acceptance_policy_hit(ctx, command) {
        notes.push(format!(
            "\nacceptance {index}: not run — {}: {command}",
            hit.reason
        ));
        return Frozen::Empty;
    }
    match safety::classify(command) {
        safety::Verdict::Safe => {}
        safety::Verdict::Blocked(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — touches protected path ({reason})"
            ));
            return Frozen::Empty;
        }
        safety::Verdict::NeedsApproval(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — would need approval ({reason}); \
                 acceptance commands run unattended, so they must be safe"
            ));
            return Frozen::Empty;
        }
    }
    let state_before = plan::state_digest(&ctx.root, paths, command);
    let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if run.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let state_after = plan::state_digest(&ctx.root, paths, command);
    let Some(exit) = run.exit_code else {
        notes.push(format!(
            "\nacceptance {index}: could not be run — {}",
            run.output.lines().next().unwrap_or("no result")
        ));
        return Frozen::Empty;
    };
    if check_never_started(exit) {
        notes.push(format!(
            "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
        ));
        return Frozen::Empty;
    }
    if state_before != state_after {
        notes.push(format!(
            "\nacceptance {index}: ran while tracked state moved; nothing frozen"
        ));
        return Frozen::Empty;
    }
    let body: String = run
        .output
        .lines()
        .filter(|line| !line.starts_with("(exit code"))
        .collect::<Vec<_>>()
        .join("\n");
    // `exec` substitutes its own "no output" placeholder for silent runs —
    // that is the runner talking, not the check, so it freezes nothing
    if body.trim().is_empty() || body.trim() == "no output" {
        notes.push(format!(
            "\nacceptance {index}: froze empty output — that discriminates nothing; \
             use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
    let head: String = body.chars().take(BASELINE_HEAD_CHARS).collect();
    let first_line: String = head
        .lines()
        .next()
        .unwrap_or("no output")
        .trim()
        .chars()
        .take(BASELINE_REASON_CHARS)
        .collect();
    notes.push(format!(
        "\nacceptance {index}: frozen (exit {exit}) — {first_line}"
    ));
    Frozen::Kept(plan::Snapshot {
        at: plan::now(),
        exit: Some(exit),
        check_definition_hash: plan::check_definition_hash(command),
        output_hash,
        head,
        state_digest: state_after,
    })
}

/// Rung 3: freeze one `differential:` item's pre-change output. The same
/// host-run rules as [`freeze_snapshot`], plus one of its own: the check
/// runs *twice* and both runs must agree byte for byte. A differential
/// settles on changed output, so a nondeterministic input would pass
/// trivially — the double run proves the input is stable enough to compare.
/// Costs two executions at plan time; that is the rung's price.
pub(crate) fn freeze_differential(
    ctx: &mut ToolCtx,
    paths: &[String],
    index: usize,
    command: &str,
    notes: &mut Vec<String>,
) -> Frozen {
    // user's hard blocks and the exfil gate first; the classifier below
    // keeps its own refusal shape
    if let Some(hit) = acceptance_policy_hit(ctx, command) {
        notes.push(format!(
            "\nacceptance {index}: not run — {}: {command}",
            hit.reason
        ));
        return Frozen::Empty;
    }
    match safety::classify(command) {
        safety::Verdict::Safe => {}
        safety::Verdict::Blocked(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — touches protected path ({reason})"
            ));
            return Frozen::Empty;
        }
        safety::Verdict::NeedsApproval(reason) => {
            notes.push(format!(
                "\nacceptance {index}: not run — would need approval ({reason}); \
                 acceptance commands run unattended, so they must be safe"
            ));
            return Frozen::Empty;
        }
    }
    let state_before = plan::state_digest(&ctx.root, paths, command);
    let first = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if first.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let second = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
    if second.cancelled {
        notes.push(format!("\nacceptance {index}: cancelled"));
        return Frozen::Cancelled;
    }
    let state_after = plan::state_digest(&ctx.root, paths, command);
    let (Some(exit), Some(second_exit)) = (first.exit_code, second.exit_code) else {
        notes.push(format!("\nacceptance {index}: could not be run"));
        return Frozen::Empty;
    };
    if check_never_started(exit) || check_never_started(second_exit) {
        notes.push(format!(
            "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
        ));
        return Frozen::Empty;
    }
    if state_before != state_after {
        notes.push(format!(
            "\nacceptance {index}: ran while tracked state moved; nothing frozen"
        ));
        return Frozen::Empty;
    }
    // raw outputs carry the "(exit code N)" footer, so this one comparison
    // covers stdout, stderr, and the exit code together
    if first.output != second.output {
        notes.push(format!(
            "\nacceptance {index}: output differs run to run — differential needs \
             a deterministic input; stabilize it or use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let body: String = first
        .output
        .lines()
        .filter(|line| !line.starts_with("(exit code"))
        .collect::<Vec<_>>()
        .join("\n");
    // same vacuity rule as a snapshot: silent output discriminates nothing
    if body.trim().is_empty() || body.trim() == "no output" {
        notes.push(format!(
            "\nacceptance {index}: froze empty output — that discriminates nothing; \
             use cmd: for a pass/fail check"
        ));
        return Frozen::Empty;
    }
    let output_hash = blake3::hash(first.output.as_bytes()).to_hex().to_string();
    let head: String = body.chars().take(BASELINE_HEAD_CHARS).collect();
    let first_line: String = head
        .lines()
        .next()
        .unwrap_or("no output")
        .trim()
        .chars()
        .take(BASELINE_REASON_CHARS)
        .collect();
    notes.push(format!(
        "\nacceptance {index}: frozen differential, stable across 2 runs (exit {exit}) — {first_line}"
    ));
    Frozen::Kept(plan::Snapshot {
        at: plan::now(),
        exit: Some(exit),
        check_definition_hash: plan::check_definition_hash(command),
        output_hash,
        head,
        state_digest: state_after,
    })
}

/// Outcome of freezing one `signatures:` item. No cancellation arm:
/// reading files is fast and has no user-cancel point, unlike executions.
pub(crate) enum Shaped {
    Kept(plan::ShapeFreeze),
    Empty,
}

/// Files above this are not shape-read: declaration outlines are for
/// source, not dumps. Mirrors the `outline` tool's ceiling.
const SHAPE_MAX_BYTES: u64 = 512_000;

/// Rung 5: freeze the declaration shapes of the named files. No commands
/// run — the host only reads — so there is no safety classification and
/// no typo guard; the failure modes are missing/unreadable files instead.
/// All-or-nothing: one bad path leaves the whole item unfrozen with a note
/// naming it, so a typo cannot silently narrow the commitment.
pub(crate) fn freeze_shapes(ctx: &mut ToolCtx, index: usize, paths: &str, notes: &mut Vec<String>) -> Shaped {
    let names = plan::signature_paths(paths);
    if names.is_empty() {
        notes.push(format!(
            "\nacceptance {index}: names no files — a signatures item without paths settles nothing"
        ));
        return Shaped::Empty;
    }
    let mut files = Vec::with_capacity(names.len());
    let mut total_items = 0usize;
    let mut summary = Vec::with_capacity(names.len());
    for name in &names {
        let full = match ctx.resolve(name) {
            Ok(full) => full,
            Err(message) => {
                notes.push(format!("\nacceptance {index}: {message}"));
                return Shaped::Empty;
            }
        };
        let meta = match std::fs::metadata(&full) {
            Ok(meta) => meta,
            Err(_) => {
                notes.push(format!(
                    "\nacceptance {index}: '{name}' is missing — freeze the files before the work starts"
                ));
                return Shaped::Empty;
            }
        };
        if meta.is_dir() {
            notes.push(format!(
                "\nacceptance {index}: '{name}' is a directory — name source files"
            ));
            return Shaped::Empty;
        }
        if meta.len() > SHAPE_MAX_BYTES {
            notes.push(format!(
                "\nacceptance {index}: '{name}' is too large to shape-read"
            ));
            return Shaped::Empty;
        }
        let bytes = match std::fs::read(&full) {
            Ok(bytes) => bytes,
            Err(e) => {
                notes.push(format!("\nacceptance {index}: cannot read '{name}': {e}"));
                return Shaped::Empty;
            }
        };
        let src = match std::str::from_utf8(&bytes) {
            Ok(src) => src,
            Err(_) => {
                notes.push(format!(
                    "\nacceptance {index}: '{name}' is binary or not valid UTF-8"
                ));
                return Shaped::Empty;
            }
        };
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
        let (parser, shape) = crate::agent::tools::outline::shape_of(src, ext);
        let shape_hash = blake3::hash(shape.join("\n").as_bytes()).to_hex().to_string();
        total_items += shape.len();
        summary.push(format!("{name} ({parser}, {})", shape.len()));
        files.push(plan::ShapeFile {
            path: name.to_string(),
            parser,
            shape_hash,
            items: shape.len(),
        });
    }
    notes.push(format!(
        "\nacceptance {index}: froze {} file(s), {total_items} declaration(s) — {}",
        files.len(),
        summary.join(", ")
    ));
    Shaped::Kept(plan::ShapeFreeze {
        at: plan::now(),
        check_definition_hash: plan::check_definition_hash(paths),
        files,
    })
}

/// Re-read the shapes named by a frozen rung-5 record: `(path, hash)` per
/// file, `None` where the file no longer reads. A deleted file is a
/// changed shape, not an error — removal breaks a freeze like any edit.
pub(crate) fn read_shapes(ctx: &ToolCtx, files: &[plan::ShapeFile]) -> Vec<(String, Option<String>)> {
    files
        .iter()
        .map(|file| {
            let hash = ctx.resolve(&file.path).ok().and_then(|full| {
                std::fs::read(&full).ok().and_then(|bytes| {
                    std::str::from_utf8(&bytes).ok().map(|src| {
                        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
                        let (_, shape) = crate::agent::tools::outline::shape_of(src, ext);
                        blake3::hash(shape.join("\n").as_bytes()).to_hex().to_string()
                    })
                })
            });
            (file.path.clone(), hash)
        })
        .collect()
}

/// §12.12: run every `cmd:` acceptance item once and keep the runs that
/// failed. Called at plan creation — the only moment the pre-change tree is
/// still the current one. Once the work starts there is nothing left to prove
/// that the check can fail at all, and a check that cannot fail settles
/// nothing.
///
/// Nothing here is fatal. A check that already passes, a command that cannot
/// run, an unsafe command: each just leaves its item without a baseline. The
/// item is shown in that state and can never be settled from it; the refusal
/// happens at `plan verify`, where the model can do something about it.
pub(crate) fn capture_baselines(ctx: &mut ToolCtx, plan: &plan::Plan) -> BaselineProof {
    // pre-sized: every item already has its three slots, so each branch
    // assigns by index and a cancelled capture just stops.
    let mut proof = BaselineProof {
        slots: vec![None; plan.acceptance.len()],
        frozen: vec![None; plan.acceptance.len()],
        shapes: vec![None; plan.acceptance.len()],
        inputs: vec![Vec::new(); plan.acceptance.len()],
        notes: Vec::new(),
    };
    // check inputs freeze once, before any check runs: hashing is
    // read-only, identical for every item, and the tree is pre-change
    // only at this moment.
    let frozen_inputs = plan::freeze_check_inputs(&ctx.root);
    let paths = plan::digest_paths(plan);
    for (index, item) in plan.acceptance.iter().enumerate() {
        if matches!(
            item.kind(),
            plan::AcceptanceKind::Command(_)
                | plan::AcceptanceKind::Snapshot(_)
                | plan::AcceptanceKind::Differential(_)
        ) {
            proof.inputs[index] = frozen_inputs.clone();
        }
        // rungs 3 and 4 freeze beside the baselines: the same moment, the
        // same host-run rules, the same positional slots. Only the verdict
        // is inverted — and rung 3 proves determinism with a double run.
        let frozen_command = match item.kind() {
            plan::AcceptanceKind::Snapshot(command) => Some((false, command.to_string())),
            plan::AcceptanceKind::Differential(command) => Some((true, command.to_string())),
            _ => None,
        };
        if let Some((is_differential, command)) = frozen_command {
            let frozen = if is_differential {
                freeze_differential(ctx, &paths, index, &command, &mut proof.notes)
            } else {
                freeze_snapshot(ctx, &paths, index, &command, &mut proof.notes)
            };
            match frozen {
                Frozen::Kept(snapshot) => proof.frozen[index] = Some(snapshot),
                Frozen::Empty => {}
                // the user is stopping the turn; later items keep their
                // pre-sized empty slots
                Frozen::Cancelled => break,
            }
            continue;
        }
        // rung 5 freezes declaration shapes instead of command output.
        if let plan::AcceptanceKind::Signatures(paths) = item.kind() {
            let paths = paths.join(", ");
            match freeze_shapes(ctx, index, &paths, &mut proof.notes) {
                Shaped::Kept(shape) => proof.shapes[index] = Some(shape),
                Shaped::Empty => {}
            }
            continue;
        }
        let plan::AcceptanceKind::Command(command) = item.kind() else {
            continue;
        };
        let command = command.to_string();
        // The command text arrives from the model and is about to be run
        // without asking, so anything that would need approval is skipped
        // rather than run — the same refusal `plan verify` makes, moved to
        // where the model can still rewrite the item.
        // User hard blocks and the exfil gate refuse first (a project-
        // injected `cmd: $name` faces the same list as a typed command).
        if let Some(hit) = acceptance_policy_hit(ctx, &command) {
            proof.notes.push(format!(
                "\nacceptance {index}: not run — {}: {command}",
                hit.reason
            ));
            continue;
        }
        match safety::classify(&command) {
            safety::Verdict::Safe => {}
            safety::Verdict::Blocked(reason) => {
                proof.notes.push(format!(
                    "\nacceptance {index}: not run — touches protected path ({reason})"
                ));
                continue;
            }
            safety::Verdict::NeedsApproval(reason) => {
                proof.notes.push(format!(
                    "\nacceptance {index}: not run — would need approval ({reason}); \
                     acceptance commands run unattended, so they must be safe"
                ));
                continue;
            }
        }
        let state_before = plan::state_digest(&ctx.root, &paths, &command);
        let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
        if run.cancelled {
            proof.notes.push(format!("\nacceptance {index}: cancelled"));
            // the user is stopping the turn; later items keep their
            // pre-sized empty slots
            break;
        }
        let state_after = plan::state_digest(&ctx.root, &paths, &command);
        let Some(exit) = run.exit_code else {
            proof.notes.push(format!(
                "\nacceptance {index}: could not be run — {}",
                run.output.lines().next().unwrap_or("no result")
            ));
            continue;
        };
        if exit == 0 {
            proof.notes.push(format!(
                "\nacceptance {index}: passes already — that makes it a regression \
                 guard, not acceptance, and it will never settle this item"
            ));
            continue;
        }
        // The shell saying the check never started, rather than the check
        // failing. Recording one of those as a baseline would make the proof
        // meaningless, so a typo stays a typo instead of becoming evidence.
        if check_never_started(exit) {
            proof.notes.push(format!(
                "\nacceptance {index}: could not be run (exit {exit}) — check the command text"
            ));
            continue;
        }
        if state_before != state_after {
            proof.notes.push(format!(
                "\nacceptance {index}: ran while tracked state moved; no baseline taken"
            ));
            continue;
        }
        let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
        let head: String = run
            .output
            .lines()
            .filter(|line| !line.starts_with("(exit code"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(BASELINE_HEAD_CHARS)
            .collect();
        // The reason travels with the verdict: on a shell that reports a typo
        // as an ordinary failure, this line is the only thing that separates
        // "the feature is missing" from "the command does not exist".
        let first_line: String = head
            .lines()
            .next()
            .unwrap_or("no output")
            .trim()
            .chars()
            .take(BASELINE_REASON_CHARS)
            .collect();
        proof.slots[index] = Some(plan::Baseline {
            at: plan::now(),
            exit,
            check_definition_hash: plan::check_definition_hash(&command),
            output_hash,
            head,
            state_digest: state_after,
        });
        proof.notes.push(format!(
            "\nacceptance {index}: fails before the change (exit {exit}) — {first_line}"
        ));
    }
    // ladder walk (§12.12): one trailing line saying where on the ladder
    // this plan stands. Rides every create/accept message for free, since
    // both append these notes raw.
    proof.notes.push(plan::ladder_note(plan));
    proof
}

/// §12.12 three states: same check, prior green receipt, same digest,
/// opposite outcome — the runs disagree, so the item is flaky rather than
/// failed. A red run on moved state is an ordinary regression
/// (`acceptance_failed`); only an attested state that now answers
/// differently proves the check itself untrustworthy.
///
/// The digest is the host's whole visibility: a change it cannot see (an
/// untracked file) reads as a flake, honestly — the host truly cannot tell
/// those apart. Tracking what the check depends on in step refs is what
/// keeps genuine regressions out of this verdict.
pub(crate) fn same_state_disagreement(item: &plan::Acceptance, command: &str, state_digest: &str) -> bool {
    if item.status != plan::AcceptanceStatus::Passed {
        return false;
    }
    let hash = plan::check_definition_hash(command);
    item.validation
        .receipts
        .iter()
        .rev()
        .find(|receipt| receipt.check_definition_hash.as_deref() == Some(hash.as_str()))
        .and_then(|receipt| receipt.state_after.as_deref())
        == Some(state_digest)
}

/// Receipt-time check-inputs verdict, shared by every host-run acceptance
/// path (`cmd:`, `snapshot:`, `differential:`, at `verify` and at
/// `complete`). Re-hashes the inputs frozen at plan time: a changed or
/// vanished file means the check no longer runs against what was frozen —
/// usually edited tests — so no verdict may issue. Records the event in
/// the journal and returns the rejection message; `None` means clean.
/// Waived items skip: the user took responsibility with the waiver.
pub(crate) fn inputs_verdict(
    ctx: &mut ToolCtx,
    item: &plan::Acceptance,
    index: usize,
) -> Result<Option<String>, String> {
    if item.status == plan::AcceptanceStatus::Waived {
        return Ok(None);
    }
    let changed = plan::changed_check_inputs(&ctx.root, &item.inputs);
    if changed.is_empty() {
        return Ok(None);
    }
    let text = format!(
        "acceptance {index} check inputs changed since plan time: {}",
        changed.join(", ")
    );
    match crate::agent::journal::Journal::open(&ctx.root, &ctx.session_id) {
        Ok(mut journal) => {
            let _ = journal.append(
                "note",
                serde_json::json!({
                    "by": "host",
                    "note": "blocker",
                    "text": text,
                }),
            );
        }
        Err(e) => return Err(format!("receipt journal unwritable: {e:#}")),
    }
    Ok(Some(format!(
        "inputs_changed: {text} — restore the frozen files, or have the user \
         waive the item with /plan waive"
    )))
}

/// Refusal for a flaky item (§12.12): reported as such, never silently
/// retried into verified. Waiver is the way out.
pub(crate) fn flaky_rejection(index: usize, command: &str) -> plan::Rejection {    plan::Rejection {
        code: "flaky_check",
        reason: format!("acceptance {index} runs disagree on the same state: {command}"),
        hint: "a green run and a red run attested the same digest, so the check \
               is flaky rather than failed: make it deterministic, or have the user \
               waive the item with /plan waive"
            .to_string(),
    }
}

/// Interval receipt for a host-run check (§2.1.4): equal before/after
/// digests, exec runner, and a matching journal record. Shared by the
/// `cmd:` and `snapshot:` verify paths so both runners attest identically.
#[allow(clippy::too_many_arguments)]
pub(crate) fn issue_exec_receipt(
    ctx: &mut ToolCtx,
    index: usize,
    command: &str,
    runner: &str,
    started_at: String,
    finished_at: String,
    state_before: String,
    state_after: String,
    paths: Vec<String>,
    exit_code: Option<i32>,
    output_hash: String,
) -> Result<plan::Receipt, String> {
    let receipt_fields = serde_json::json!({
        "check_definition_hash": plan::check_definition_hash(command),
        "runner": runner,
        "command": command,
        "args": serde_json::Value::Null,
        "cwd": ctx.root.display().to_string(),
        "started_at": started_at,
        "finished_at": finished_at,
        "state_before": state_before,
        "state_after": state_after,
        "exit": exit_code,
        "output_hash": output_hash,
        "paths": paths,
    });
    let seq = match crate::agent::journal::Journal::open(&ctx.root, &ctx.session_id) {
        Ok(mut journal) => {
            match journal.append_verification_receipt(index, receipt_fields) {
                Ok(seq) => seq,
                Err(e) => {
                    return Err(format!("receipt journal unwritable: {e:#}"));
                }
            }
        }
        Err(e) => {
            return Err(format!("receipt journal unwritable: {e:#}"));
        }
    };
    Ok(plan::Receipt {
        session: ctx.session_id.clone(),
        seq,
        state_digest: state_after.clone(),
        command: Some(command.to_string()),
        exit: exit_code,
        at: finished_at.clone(),
        check_definition_hash: Some(plan::check_definition_hash(command)),
        runner: Some(runner.to_string()),
        args: None,
        cwd: Some(ctx.root.display().to_string()),
        started_at: Some(started_at),
        finished_at: Some(finished_at),
        state_before: Some(state_before),
        state_after: Some(state_after),
        output_hash: Some(output_hash),
        paths,
    })
}

/// `plan verify <index>` — the host settles the item, on its own terms.
///
/// A `cmd:` item is run here and now (§2.1.2: "host runs it on `plan verify`
/// and on `complete`"). A `Text` item needs host-recorded evidence that no
/// other acceptance item has already spent. A `manual:` item is refused: only
/// the user waives those.
pub(crate) fn verify_acceptance(ctx: &mut ToolCtx, index: usize, supplied: bool) -> Outcome {
    let mut active = match plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Outcome::err("no active plan: create one with op=create first"),
        Err(e) => return Outcome::err(format!("plan store unreadable: {e:#}")),
    };
    let Some(item) = active.acceptance.get(index) else {
        return rejection(plan::Rejection {
            code: "unknown_acceptance",
            reason: format!("no acceptance item {index}"),
            hint: "call plan show to see the acceptance list".to_string(),
        });
    };

    // §12.12 three states: a flaky item is reported, never silently retried
    // into verified — no run is spent here, whatever the item's kind.
    if item.validation.status == plan::ValidationStatus::Unknown {
        return rejection(flaky_rejection(index, &item.text));
    }

    // transparency for the impact fast path in the Command arm: what
    // ran instead of the authored command, if anything
    let mut impact_note: Option<String> = None;
    let (evidence, receipt) = match item.kind() {
        plan::AcceptanceKind::Manual(_) => (Vec::new(), None),
        plan::AcceptanceKind::Command(command) => {
            let command = command.to_string();
            // §12.12: a green run only means something if red was possible.
            // This is a rule about what the host may *accept*, so it sits here
            // rather than in `plan::verify_acceptance`, which replay also
            // drives — and replay restores commits that were already accepted,
            // so it must not re-judge them.
            if !plan::proven_failing(item) {
                let hint = match item.baseline.as_ref() {
                    Some(baseline) => format!(
                        "the check was rewritten after its baseline was taken \
                         (failed with exit {} at {}); prove the new text fails on \
                         the pre-change state, or have the user waive the item \
                         with /plan waive",
                        baseline.exit, baseline.at
                    ),
                    None => "no run of this check has ever failed here, so passing \
                             it proves nothing: make it reproduce the problem before \
                             the work starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_baseline",
                    reason: format!(
                        "acceptance {index} has no proof that it can fail: {command}"
                    ),
                    hint,
                });
            }
            // frozen check inputs first: a verdict on edited tests settles
            // nothing, whichever way the run goes.
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
            // The acceptance text arrives from the model on `plan create`, so
            // it is model-controlled input that the host is about to execute.
            // It goes through the same classifier as `bash`, and anything that
            // would need approval is refused rather than silently run: an
            // acceptance criterion is not the place to ask.
            // User hard blocks and the exfil gate refuse with their own codes.
            if let Some(hit) = acceptance_policy_hit(ctx, &command) {
                return rejection(plan::Rejection {
                    code: hit.code,
                    reason: format!("acceptance {index} {}: {command}", hit.reason),
                    hint: hit.hint.to_string(),
                });
            }
            match safety::classify(&command) {
                safety::Verdict::Blocked(reason) => {
                    return rejection(plan::Rejection {
                        code: "protected_path",
                        reason: format!(
                            "acceptance {index} touches protected path ({reason}): {command}"
                        ),
                        hint: "acceptance commands must not touch host-owned state".to_string(),
                    });
                }
                safety::Verdict::NeedsApproval(reason) => {
                    return rejection(plan::Rejection {
                        code: "unsafe_acceptance",
                        reason: format!("acceptance {index} would run a {reason} command: {command}"),
                        hint: "acceptance commands run without asking, so they must be safe;                            rewrite it or have the user waive the item"
                            .to_string(),
                    });
                }
                safety::Verdict::Safe => {}
            }
            // interval consistency (§2.1.4): digest the traversed state
            // before AND after the run. A mutation mid-check (background
            // job, concurrent subagent) means the check proved nothing —
            // no receipt is issued and the item stays unverified.
            let paths = plan::digest_paths(&active);
            // Test-impact fast path (§2.4.11): a bare test-runner
            // invocation runs the tests covering the changed files
            // first. The receipt records what actually ran, and
            // `complete` still runs the full suite — so a green verify
            // means the covering tests passed, while the suite-wide
            // verdict stays with `complete`. Anything unrecognized
            // runs the authored command unchanged.
            let impact = crate::agent::test_impact::select_command(
                &ctx.root,
                &active.id,
                &command,
            );
            let run_command = impact
                .as_ref()
                .map(|selected| selected.command.clone())
                .unwrap_or_else(|| command.clone());
            impact_note = impact.as_ref().map(|selected| selected.note.clone());
            let state_before = plan::state_digest(&ctx.root, &paths, &run_command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &run_command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
            if !run.ok {
                let state_after = plan::state_digest(&ctx.root, &paths, &run_command);
                // same attested state, opposite outcome: flaky, not failed
                if state_before == state_after
                    && same_state_disagreement(&active.acceptance[index], &run_command, &state_after)
                {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": run_command,
                            "state_digest": state_after,
                        });
                        if let Err(e) = plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        ) {
                            return Outcome::err(format!("plan write failed: {e:#}"));
                        }
                    }
                    return rejection(flaky_rejection(index, &run_command));
                }
                return rejection(plan::Rejection {
                    code: "acceptance_failed",
                    reason: format!("acceptance {index} command failed: {run_command}"),
                    hint: format!(
                        "fix what it reports, then verify again — {}",
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }
            let state_after = plan::state_digest(&ctx.root, &paths, &run_command);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
            // exit travels with the outcome now: receipts only issue on
            // `ok` runs, so this is zero in practice, but sourced rather
            // than assumed — a nonzero code with ok would be a loud bug,
            // not a silent receipt
            let exit_code = run.exit_code;
            let receipt = match issue_exec_receipt(
                ctx,
                index,
                &run_command,
                "exec",
                started_at,
                finished_at,
                state_before,
                state_after,
                paths,
                exit_code,
                output_hash,
            ) {
                Ok(receipt) => receipt,
                Err(message) => return Outcome::err(message),
            };
            (Vec::new(), Some(receipt))
        }
        plan::AcceptanceKind::Snapshot(command) => {
            let command = command.to_string();
            // rung 4 settles on frozen behavior, not on failure: the host
            // must have frozen this exact check at plan time. Like the
            // baseline gate this lives at the host boundary so replay of
            // accepted commits never re-judges them.
            if !plan::snapshot_current(item) {
                let hint = match item.snapshot.as_ref() {
                    Some(frozen) => format!(
                        "the check was rewritten after its output was frozen \
                         (at {}); freeze the new text on the pre-change state, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no run of this check was ever frozen here, so no output \
                             can match it: freeze the behavior before the work \
                             starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_snapshot",
                    reason: format!(
                        "acceptance {index} has no frozen output to compare to: {command}"
                    ),
                    hint,
                });
            }
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
            let frozen = item.snapshot.as_ref().expect("gated above");
            // model-controlled input, same classifier as `bash`: frozen or
            // not, an unsafe check never runs unattended.
            // User hard blocks and the exfil gate refuse with their own codes.
            if let Some(hit) = acceptance_policy_hit(ctx, &command) {
                return rejection(plan::Rejection {
                    code: hit.code,
                    reason: format!("acceptance {index} {}: {command}", hit.reason),
                    hint: hit.hint.to_string(),
                });
            }
            match safety::classify(&command) {
                safety::Verdict::Blocked(reason) => {
                    return rejection(plan::Rejection {
                        code: "protected_path",
                        reason: format!(
                            "acceptance {index} touches protected path ({reason}): {command}"
                        ),
                        hint: "acceptance commands must not touch host-owned state".to_string(),
                    });
                }
                safety::Verdict::NeedsApproval(reason) => {
                    return rejection(plan::Rejection {
                        code: "unsafe_acceptance",
                        reason: format!("acceptance {index} would run a {reason} command: {command}"),
                        hint: "acceptance commands run without asking, so they must be safe;                            rewrite it or have the user waive the item"
                            .to_string(),
                    });
                }
                safety::Verdict::Safe => {}
            }
            let paths = plan::digest_paths(&active);
            let state_before = plan::state_digest(&ctx.root, &paths, &command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
            let state_after = plan::state_digest(&ctx.root, &paths, &command);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
            if output_hash == frozen.output_hash && run.exit_code == frozen.exit {
                let receipt = match issue_exec_receipt(
                    ctx,
                    index,
                    &command,
                    "exec",
                    started_at,
                    finished_at,
                    state_before,
                    state_after,
                    paths,
                    run.exit_code,
                    output_hash,
                ) {
                    Ok(receipt) => receipt,
                    Err(message) => return Outcome::err(message),
                };
                (Vec::new(), Some(receipt))
            } else {
                // the output moved: nondeterministic on the same state is a
                // flake, changed on moved state is changed behavior
                if same_state_disagreement(&active.acceptance[index], &command, &state_after) {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": command,
                            "state_digest": state_after,
                        });
                        if let Err(e) = plan::commit(
                            &ctx.root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        ) {
                            return Outcome::err(format!("plan write failed: {e:#}"));
                        }
                    }
                    return rejection(flaky_rejection(index, &command));
                }
                return rejection(plan::Rejection {
                    code: "snapshot_changed",
                    reason: format!("acceptance {index} output no longer matches: {command}"),
                    hint: format!(
                        "frozen at {} (exit {:?}); now exit {:?} — {}",
                        frozen.at,
                        frozen.exit,
                        run.exit_code,
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }
        }
        plan::AcceptanceKind::Differential(command) => {
            let command = command.to_string();
            // rung 3 settles on *changed* output against the same frozen
            // record rung 4 compares for equality. The freeze gate is the
            // same shape with the inverted meaning.
            if !plan::differential_current(item) {
                let hint = match item.snapshot.as_ref() {
                    Some(frozen) => format!(
                        "the check was rewritten after its output was frozen \
                         (at {}); freeze the new text on the pre-change state, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no run of this check was ever frozen here, so no output \
                             can be compared: freeze the behavior before the work \
                             starts, or have the user waive the item with \
                             /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_snapshot",
                    reason: format!(
                        "acceptance {index} has no frozen output to compare to: {command}"
                    ),
                    hint,
                });
            }
            match inputs_verdict(ctx, item, index) {
                Err(message) => return Outcome::err(message),
                Ok(Some(message)) => return Outcome::err(message),
                Ok(None) => {}
            }
            let frozen = item.snapshot.as_ref().expect("gated above");
            // User hard blocks and the exfil gate refuse with their own codes.
            if let Some(hit) = acceptance_policy_hit(ctx, &command) {
                return rejection(plan::Rejection {
                    code: hit.code,
                    reason: format!("acceptance {index} {}: {command}", hit.reason),
                    hint: hit.hint.to_string(),
                });
            }
            match safety::classify(&command) {
                safety::Verdict::Blocked(reason) => {
                    return rejection(plan::Rejection {
                        code: "protected_path",
                        reason: format!(
                            "acceptance {index} touches protected path ({reason}): {command}"
                        ),
                        hint: "acceptance commands must not touch host-owned state".to_string(),
                    });
                }
                safety::Verdict::NeedsApproval(reason) => {
                    return rejection(plan::Rejection {
                        code: "unsafe_acceptance",
                        reason: format!("acceptance {index} would run a {reason} command: {command}"),
                        hint: "acceptance commands run without asking, so they must be safe;                            rewrite it or have the user waive the item"
                            .to_string(),
                    });
                }
                safety::Verdict::Safe => {}
            }
            let paths = plan::digest_paths(&active);
            let state_before = plan::state_digest(&ctx.root, &paths, &command);
            let started_at = plan::now();
            let run = exec::bash(ctx, &command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
            let finished_at = plan::now();
            let state_after = plan::state_digest(&ctx.root, &paths, &command);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
            if output_hash == frozen.output_hash && run.exit_code == frozen.exit {
                return rejection(plan::Rejection {
                    code: "no_observable_change",
                    reason: format!(
                        "acceptance {index} output identical to the pre-change run: {command}"
                    ),
                    hint: "the change has no observable effect through this input — pick \
                           an input the work actually moves, or use snapshot: if the \
                           behavior must not move"
                        .to_string(),
                });
            }
            // changed output settles only a working change: a command that
            // now exits non-zero broke the input, it did not move it.
            if run.exit_code != Some(0) {
                return rejection(plan::Rejection {
                    code: "broken_change",
                    reason: format!(
                        "acceptance {index} output changed but exits {:?}: {command}",
                        run.exit_code
                    ),
                    hint: "differential settles behavior that changed and works — fix \
                           the breakage, or have the user waive the item with \
                           /plan waive"
                        .to_string(),
                });
            }
            // observably changed through this input, and working: the
            // work moved the behavior without breaking the check
            let receipt = match issue_exec_receipt(
                ctx,
                index,
                &command,
                "exec",
                started_at,
                finished_at,
                state_before,
                state_after,
                paths,
                run.exit_code,
                output_hash,
            ) {
                Ok(receipt) => receipt,
                Err(message) => return Outcome::err(message),
            };
            (Vec::new(), Some(receipt))
        }
        plan::AcceptanceKind::Signatures(paths) => {
            let canonical = paths.join(", ");
            // rung 5 settles on frozen declaration shapes: nothing was
            // executed, so there is no baseline to demand — only the freeze.
            // Like the output gates this lives at the host boundary so
            // replay never re-judges accepted commits.
            if !plan::signatures_current(item) {
                let hint = match item.shape.as_ref() {
                    Some(frozen) => format!(
                        "the file set was renamed after its shapes were frozen \
                         (at {}); freeze the new set on the pre-change tree, \
                         or have the user waive the item with /plan waive",
                        frozen.at
                    ),
                    None => "no shapes were ever frozen here, so nothing can be \
                             compared: freeze the files before the work starts, \
                             or have the user waive the item with /plan waive"
                        .to_string(),
                };
                return rejection(plan::Rejection {
                    code: "no_signatures",
                    reason: format!(
                        "acceptance {index} has no frozen shapes to compare to: {canonical}"
                    ),
                    hint,
                });
            }
            let frozen = item.shape.as_ref().expect("gated above");
            // the digest covers the named files plus the plan refs, so a
            // concurrent edit cannot slip between the re-reads unnoticed
            let mut shape_paths = plan::digest_paths(&active);
            for file in &frozen.files {
                if !shape_paths.contains(&file.path) {
                    shape_paths.push(file.path.clone());
                }
            }
            let state_before = plan::state_digest(&ctx.root, &shape_paths, &canonical);
            let started_at = plan::now();
            let current = read_shapes(ctx, &frozen.files);
            let finished_at = plan::now();
            let state_after = plan::state_digest(&ctx.root, &shape_paths, &canonical);
            if state_before != state_after {
                return rejection(plan::Rejection {
                    code: "state_changed_during_check",
                    reason: format!(
                        "acceptance {index} ran while tracked state moved; no receipt issued"
                    ),
                    hint: "run verify again on the settled state".to_string(),
                });
            }
            let changed: Vec<&str> = frozen
                .files
                .iter()
                .zip(current.iter())
                .filter(|(frozen_file, (_, hash))| {
                    hash.as_deref() != Some(frozen_file.shape_hash.as_str())
                })
                .map(|(frozen_file, _)| frozen_file.path.as_str())
                .collect();
            if changed.is_empty() {
                let output_hash = blake3::hash(
                    current
                        .iter()
                        .map(|(_, hash)| hash.as_deref().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .as_bytes(),
                )
                .to_hex()
                .to_string();
                let receipt = match issue_exec_receipt(
                    ctx,
                    index,
                    &canonical,
                    "ast",
                    started_at,
                    finished_at,
                    state_before,
                    state_after,
                    shape_paths,
                    None,
                    output_hash,
                ) {
                    Ok(receipt) => receipt,
                    Err(message) => return Outcome::err(message),
                };
                (Vec::new(), Some(receipt))
            } else {
                return rejection(plan::Rejection {
                    code: "signatures_changed",
                    reason: format!(
                        "acceptance {index} declaration shapes changed: {}",
                        changed.join(", ")
                    ),
                    hint: "restore the declared structure, or have the user waive \
                           the item with /plan waive"
                        .to_string(),
                });
            }
        }
        plan::AcceptanceKind::Text(text) => {
            // Untyped acceptance cannot be created anymore; files written
            // before the gate still load, but nothing settles them. The
            // pure layer repeats this verdict for replayed commits.
            return rejection(plan::Rejection {
                code: "untyped_acceptance",
                reason: format!("acceptance {index} is free text: {text}"),
                hint: "rewrite it as cmd: (pass/fail), snapshot: (frozen output), \
                       differential: (changed output), signatures: (file shapes), \
                       or manual: (the user checks it by hand)"
                    .to_string(),
            });
        }
    };

    match plan::verify_acceptance(&mut active, index, evidence, supplied, receipt) {
        Ok(applied) => {
            // the receipt rides the commit args so replay restores
            // validation without re-running the check
            let receipt_value = active
                .acceptance
                .get(index)
                .and_then(|item| item.validation.receipts.last())
                .and_then(|r| serde_json::to_value(r).ok());
            let mut args = serde_json::json!({
                "acceptance": index,
                "evidence_refs": active.acceptance.get(index).map(|item| item.evidence.clone()).unwrap_or_default(),
            });
            if let Some(receipt_value) = receipt_value {
                args["receipt"] = receipt_value;
            }
            if let Err(e) = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "verify",
                "model",
                true,
                args,
            ) {
                return Outcome::err(format!("plan write failed: {e:#}"));
            }
            match applied {
                plan::Applied::Updated { message } => Outcome::ok(match impact_note {
                    Some(note) => format!("{message}\n{note}"),
                    None => message,
                }),
                _ => Outcome::ok(format!("acceptance {index} verified")),
            }
        }
        Err(r) => {
            let args = serde_json::json!({"acceptance": index});
            let _ = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "verify",
                "model",
                false,
                args,
            );
            rejection(r)
        }
    }
}

/// Append the open-assumption warning to a successful `finish` (§2.1.4).
///
/// Non-blocking on purpose: the step is already finished when this runs. The
/// point is that an assumption cannot quietly outlive the step that made it —
/// the model either resolves it with `note { resolves }` or carries it
/// forward knowingly.
pub(crate) fn with_assumption_warning(ctx: &ToolCtx, finished_step: Option<&str>, message: String) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let open =
        crate::agent::journal::Journal::open_assumptions_in(&ctx.root, &ctx.session_id, Some(step))
            .unwrap_or_default();
    if open.is_empty() {
        return message;
    }
    let list = open
        .iter()
        .map(|item| item.label(80))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "{message}\nwarning: step {step} has {} open assumption(s) ({list}) — resolve with \
         note {{ kind: \"assumption\", resolves: <seq> }} or convert before completing",
        open.len()
    )
}

pub(crate) fn with_evidence_ts_warning(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let Some(s) = active.step(step) else {
        return message;
    };
    let warns = crate::agent::journal::Journal::stale_evidence_warnings(
        &ctx.root,
        &active.id,
        step,
        &s.evidence,
    );
    if warns.is_empty() {
        return message;
    }
    format!("{message}\nwarning: {}", warns.join("; "))
}

/// One-line blast radius on `finish`: the files this step wrote, from the
/// journal's `file_diff` chain (all sessions — subagent writes count).
/// Silent when the step wrote nothing (research steps). Informational,
/// not a refusal: it makes the scope visible before `complete`, where
/// the full suite still has to pass.
pub(crate) fn with_blast_radius(
    ctx: &ToolCtx,
    finished_step: Option<&str>,
    active: &plan::Plan,
    message: String,
) -> String {
    let Some(step) = finished_step else {
        return message;
    };
    let revert = crate::agent::journal::Journal::step_pre_images_in(
        &ctx.root,
        Some(&active.id),
        step,
    )
    .unwrap_or_default();
    let mut paths: Vec<String> = revert
        .files
        .iter()
        .map(|file| file.path.clone())
        .chain(revert.written_since.iter().cloned())
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return message;
    }
    const SHOW: usize = 6;
    let list = if paths.len() <= SHOW {
        paths.join(", ")
    } else {
        format!("{}, … (+{} more)", paths[..SHOW].join(", "), paths.len() - SHOW)
    };
    format!(
        "{message}\nblast radius: step {step} touched {} file(s) ({list})",
        paths.len()
    )
}

/// The gate on `plan finish`.
///
/// Soft steps (the default) close on a summary alone: the pure finish
/// validator already demands it, and progress is read from receipts, not
/// from gates. Strict mode additionally demands host-recorded evidence of
/// successful work on the step — the same content rule acceptance
/// evidence follows, so a research step can no longer close on a bare
/// failed call, and no step closes on errored diagnostics.
pub(crate) fn validate_evidence(
    root: &Path,
    op: &plan::Op,
    session_id: Option<&str>,
    strict: bool,
) -> Result<(), String> {
    let plan::Op::Finish { id, .. } = op else {
        return Ok(());
    };
    if !strict {
        return Ok(());
    };
    let status = plan::open_active_for_session(root, session_id)
        .ok()
        .flatten()
        .and_then(|p| p.step(id).map(|s| s.status));
    if status != Some(plan::StepStatus::InProgress) {
        // `finish` on a step that is not in progress is rejected by the
        // validator with a clearer reason than a missing-evidence error.
        return Ok(());
    }
    let active = plan::open_active_for_session(root, session_id)
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| "invalid_evidence: no active plan".to_string())?;
    let evidence = active
        .step(id)
        .map(|step| step.evidence.clone())
        .unwrap_or_default();
    validate_attached_records(root, &active.id, id, &evidence)
}

/// Source extensions scanned for `forbid-import:` violations. Mirrors
/// the outline/ast-grep language set.
const CONSTRAINT_SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx", "go", "sh", "bash", "c",
    "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx", "cs", "java",
];

/// Never scanned for constraint evaluation: VCS, host state, build outputs.
const CONSTRAINT_SKIP_DIRS: &[&str] = &[".git", ".sqwai", "target", "node_modules", "dist", "build"];

/// Caps mirror the `ast_grep` tool's own ceilings.
const CONSTRAINT_MAX_FILES: usize = 2_000;
const CONSTRAINT_MAX_BYTES: u64 = 512_000;
const CONSTRAINT_MAX_HITS: usize = 5;

/// Import-statement keywords across the scanned languages. A line counts
/// when it opens with one of these (after whitespace) and names the
/// pattern — plus bare quoted references (`"x/y"` import-block entries),
/// minus comment lines.
fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with('#') || t.starts_with('*') || t.starts_with("<!--")
}

pub(crate) fn import_line_references(line: &str, pattern: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "use ",
        "use\t",
        "import ",
        "import\t",
        "from ",
        "require",
        "include ",
        "mod ",
        "extern crate ",
    ];
    let trimmed = line.trim_start();
    if is_comment(line) {
        return false;
    }
    if KEYWORDS.iter().any(|kw| trimmed.starts_with(kw)) {
        return trimmed.contains(pattern);
    }
    let bare = trimmed.trim_end_matches([',', ';']);
    bare.len() > 2
        && (bare.starts_with('"') && bare.ends_with('"')
            || bare.starts_with('\'') && bare.ends_with('\''))
        && bare.contains(pattern)
}

/// `forbid-import:` violations as `path:line: text`, capped. Heuristic by
/// design (Go block imports without keywords only match as bare quoted
/// strings); the waiver covers false positives, silence would cover
/// violations.
pub(crate) fn forbid_import_violations(root: &Path, pattern: &str) -> Vec<String> {
    fn walk(
        root: &Path,
        dir: &Path,
        pattern: &str,
        out: &mut Vec<String>,
        scanned: &mut usize,
    ) {
        if out.len() >= CONSTRAINT_MAX_HITS || *scanned >= CONSTRAINT_MAX_FILES {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if out.len() >= CONSTRAINT_MAX_HITS || *scanned >= CONSTRAINT_MAX_FILES {
                return;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !CONSTRAINT_SKIP_DIRS.contains(&name.as_str()) {
                    walk(root, &path, pattern, out, scanned);
                }
                continue;
            }
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| CONSTRAINT_SOURCE_EXTS.contains(&ext));
            if !ext_ok {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > CONSTRAINT_MAX_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            *scanned += 1;
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            for (n, line) in text.lines().enumerate() {
                if import_line_references(line, pattern) {
                    out.push(format!("{}:{}: {}", rel, n + 1, line.trim()));
                    if out.len() >= CONSTRAINT_MAX_HITS {
                        return;
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut scanned = 0usize;
    walk(root, root, pattern, &mut out, &mut scanned);
    out
}

/// File paths this plan's steps recorded as written: journal `file_diff`
/// records behind the steps' evidence refs. The same host-recorded source
/// the scope guard reads — bash-written bytes stay invisible here too.
pub(crate) fn plan_file_diff_paths(root: &Path, plan: &plan::Plan) -> Vec<String> {
    let mut out = Vec::new();
    for step in &plan.steps {
        for reference in &step.evidence {
            let record = match crate::agent::journal::Journal::evidence(
                root,
                &plan.id,
                Some(&step.id),
                reference,
                None,
            ) {
                Ok(Some(record)) => record,
                _ => continue,
            };
            if record.kind == "file_diff"
                && let Some(path) = record.fields.get("path").and_then(Value::as_str)
            {
                let clean = lexical_clean(&path.replace('\\', "/"));
                if !out.contains(&clean) {
                    out.push(clean);
                }
            }
        }
    }
    out.sort();
    out
}


/// Typed-constraint verdicts at `complete`: every non-waived executable
/// constraint must hold. `forbid-cmd:` is live-gated in `bash_call` and
/// has nothing to re-check; unprefixed constraints are advisory.
pub(crate) fn validate_constraints(ctx: &mut ToolCtx, active: &plan::Plan) -> Result<(), String> {
    let waived = plan::waived_constraint_indices(active);
    for (index, text) in active.constraints.iter().enumerate() {
        if waived.contains(&index) {
            continue;
        }
        let failed: Option<String> = match plan::classify_constraint(text) {
            plan::ConstraintKind::Plain(_) => None,
            plan::ConstraintKind::ForbidCmd(_) => None,
            plan::ConstraintKind::ForbidImport(pattern) => {
                if pattern.trim().is_empty() {
                    continue;
                }
                let hits = forbid_import_violations(&ctx.root, pattern);
                (!hits.is_empty()).then(|| {
                    format!("forbidden import '{pattern}' referenced at {}", hits.join("; "))
                })
            }
            plan::ConstraintKind::Ast(pattern) => {
                if pattern.trim().is_empty() {
                    continue;
                }
                let outcome = astgrep::ast_grep(
                    ctx,
                    &serde_json::json!({"pattern": pattern, "path": ".", "max": 3}),
                );
                if !outcome.ok {
                    Some(format!("ast pattern failed: {}", outcome.output))
                } else if outcome.output.starts_with("0 matches") {
                    None
                } else {
                    let head: Vec<&str> = outcome.output.lines().skip(1).take(3).collect();
                    Some(format!(
                        "ast pattern `{pattern}` matches: {}",
                        head.join(" / ")
                    ))
                }
            }
            plan::ConstraintKind::Path(roots) => {
                if roots.is_empty() {
                    continue;
                }
                let roots: Vec<String> = roots
                    .iter()
                    .map(|r| lexical_clean(&r.replace('\\', "/")))
                    .collect();
                let outside: Vec<String> = plan_file_diff_paths(&ctx.root, active)
                    .into_iter()
                    .filter(|path| !in_write_scope(path, &roots))
                    .collect();
                (!outside.is_empty()).then(|| {
                    format!(
                        "change escapes the declared roots ({}): {}",
                        roots.join(", "),
                        outside.join(", ")
                    )
                })
            }
        };
        if let Some(details) = failed {
            return Err(format!(
                "constraint_violated: constraint {index} violated: {details} — fix it, or have the user waive it (/plan waive-constraint {index} <reason>)"
            ));
        }
    }
    Ok(())
}

/// The gate on `plan complete`.///
/// Every done step is re-checked against the journal, and every acceptance
/// item is settled again on its own terms: a `cmd:` item is **re-run** rather
/// than trusted from an earlier verify (§2.1.2 has the host run it "on `plan
/// verify` and on `complete`"; §2.4.11 makes the same point for the full test
/// suite), and a `Text` item is re-checked against the records it was verified
/// with. A waived item is the user's call and is left alone.
pub(crate) fn validate_complete(ctx: &mut ToolCtx) -> Result<(), String> {
    let root = ctx.root.clone();
    let mut active = plan::open_active_for_session(&root, Some(&ctx.session_id))
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| "invalid_evidence: no active plan".to_string())?;
    for step in active
        .steps
        .iter()
        .filter(|step| step.status == plan::StepStatus::Done)
    {
        // soft steps may close on a summary alone: nothing recorded means
        // nothing to re-check. Strict mode never gets here without evidence,
        // because its finish gate already demanded it.
        if step.evidence.is_empty() {
            continue;
        }
        validate_attached_records(&root, &active.id, &step.id, &step.evidence)?;
    }
    // typed constraints settle like acceptance: a violated executable
    // constraint blocks completion the same way a red check does.
    validate_constraints(ctx, &active)?;
    // by index: a flaky verdict below mutates the plan, which an
    // iterator borrow would not allow
    for index in 0..active.acceptance.len() {
        if active.acceptance[index].status != plan::AcceptanceStatus::Passed {
            continue;
        }
        // state moved under a recorded check after verification: the pure
        // complete gate rejects this too, but failing here names the fix
        // (re-verify) before the op is even attempted
        if active.acceptance[index].validation.status == plan::ValidationStatus::Stale {
            return Err(format!(
                "acceptance_stale: acceptance {index} went stale after verification (tracked files changed); re-verify it, then complete"
            ));
        }
        match active.acceptance[index].kind() {
            plan::AcceptanceKind::Command(command) => {
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
                // User hard blocks and the exfil gate refuse with their own codes.
                if let Some(hit) = acceptance_policy_hit(ctx, command) {
                    return Err(format!(
                        "{}: acceptance {index} {}: {command}",
                        hit.code, hit.reason
                    ));
                }
                match safety::classify(command) {
                    safety::Verdict::Blocked(reason) => {
                        return Err(format!(
                            "protected_path: acceptance {index} touches protected path ({reason}) at completion: {command}"
                        ));
                    }
                    safety::Verdict::NeedsApproval(reason) => {
                        return Err(format!(
                            "unsafe_acceptance: acceptance {index} would run a {reason} command                          at completion: {command}"
                        ));
                    }
                    safety::Verdict::Safe => {}
                }
                // the re-run is the second opinion (§2.1.2): same attested
                // state answering differently means flaky, not failed
                let paths = plan::digest_paths(&active);
                let state_before = plan::state_digest(&root, &paths, command);
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let state_after = plan::state_digest(&root, &paths, command);
                if !run.ok {
                    let command_text = command.to_string();
                    let flaky = state_before == state_after
                        && same_state_disagreement(
                            &active.acceptance[index],
                            &command_text,
                            &state_after,
                        );
                    if flaky {
                        if plan::apply_flaky(&mut active, index) {
                            let args = serde_json::json!({
                                "index": index,
                                "command": command_text,
                                "state_digest": state_after,
                            });
                            plan::commit(
                                &root,
                                &ctx.session_id,
                                &mut active,
                                "flaky",
                                "host",
                                true,
                                args,
                            )
                            .map_err(|e| format!("plan write failed: {e:#}"))?;
                        }
                        return Err(format!(
                            "flaky_check: acceptance {index} passed then failed on the same state: {command_text} — fix the flake or have the user waive it"
                        ));
                    }
                    return Err(format!(
                        "acceptance_failed: acceptance {index} no longer passes: {command_text} — {}",
                        run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                    ));
                }
            }
            plan::AcceptanceKind::Snapshot(command) => {
                // rung 4 re-runs like a command, but settles on frozen
                // output rather than on pass/fail
                if !plan::snapshot_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_snapshot: acceptance {index} has no frozen output to compare to: {command}"
                    ));
                }
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
                let frozen_hash = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .map(|frozen| frozen.output_hash.clone());
                let frozen_exit = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .and_then(|frozen| frozen.exit);
                // User hard blocks and the exfil gate refuse with their own codes.
                if let Some(hit) = acceptance_policy_hit(ctx, command) {
                    return Err(format!(
                        "{}: acceptance {index} {}: {command}",
                        hit.code, hit.reason
                    ));
                }
                match safety::classify(command) {
                    safety::Verdict::Blocked(reason) => {
                        return Err(format!(
                            "protected_path: acceptance {index} touches protected path ({reason}) at completion: {command}"
                        ));
                    }
                    safety::Verdict::NeedsApproval(reason) => {
                        return Err(format!(
                            "unsafe_acceptance: acceptance {index} would run a {reason} command                          at completion: {command}"
                        ));
                    }
                    safety::Verdict::Safe => {}
                }
                let command_text = command.to_string();
                let paths = plan::digest_paths(&active);
                let state_before = plan::state_digest(&root, &paths, command);
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let state_after = plan::state_digest(&root, &paths, command);
                let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
                if Some(output_hash.as_str()) == frozen_hash.as_deref()
                    && run.exit_code == frozen_exit
                {
                    continue;
                }
                let flaky = state_before == state_after
                    && same_state_disagreement(
                        &active.acceptance[index],
                        &command_text,
                        &state_after,
                    );
                if flaky {
                    if plan::apply_flaky(&mut active, index) {
                        let args = serde_json::json!({
                            "index": index,
                            "command": command_text,
                            "state_digest": state_after,
                        });
                        plan::commit(
                            &root,
                            &ctx.session_id,
                            &mut active,
                            "flaky",
                            "host",
                            true,
                            args,
                        )
                        .map_err(|e| format!("plan write failed: {e:#}"))?;
                    }
                    return Err(format!(
                        "flaky_check: acceptance {index} matched then differed on the same state: {command_text} — fix the flake or have the user waive it"
                    ));
                }
                return Err(format!(
                    "snapshot_changed: acceptance {index} output no longer matches: {command_text} — {}",
                    run.output.lines().take(6).collect::<Vec<_>>().join(" / ")
                ));
            }
            plan::AcceptanceKind::Differential(command) => {
                // rung 3 re-runs like a snapshot, but passes on changed
                // output and blocks on identical output
                if !plan::differential_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_snapshot: acceptance {index} has no frozen output to compare to: {command}"
                    ));
                }
                if let Some(message) = inputs_verdict(ctx, &active.acceptance[index], index)? {
                    return Err(message);
                }
                let frozen_hash = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .map(|frozen| frozen.output_hash.clone());
                let frozen_exit = active.acceptance[index]
                    .snapshot
                    .as_ref()
                    .and_then(|frozen| frozen.exit);
                // User hard blocks and the exfil gate refuse with their own codes.
                if let Some(hit) = acceptance_policy_hit(ctx, command) {
                    return Err(format!(
                        "{}: acceptance {index} {}: {command}",
                        hit.code, hit.reason
                    ));
                }
                match safety::classify(command) {
                    safety::Verdict::Blocked(reason) => {
                        return Err(format!(
                            "protected_path: acceptance {index} touches protected path ({reason}) at completion: {command}"
                        ));
                    }
                    safety::Verdict::NeedsApproval(reason) => {
                        return Err(format!(
                            "unsafe_acceptance: acceptance {index} would run a {reason} command                          at completion: {command}"
                        ));
                    }
                    safety::Verdict::Safe => {}
                }
                let command_text = command.to_string();
                let run = exec::bash(ctx, command, Some(ACCEPTANCE_TIMEOUT_SECS), false);
                let output_hash = blake3::hash(run.output.as_bytes()).to_hex().to_string();
                if frozen_hash.as_deref() == Some(output_hash.as_str())
                    && frozen_exit == run.exit_code
                {
                    return Err(format!(
                        "no_observable_change: acceptance {index} output identical to the pre-change run: {command_text}"
                    ));
                }
                // changed output settles only a working change here too
                if run.exit_code != Some(0) {
                    return Err(format!(
                        "broken_change: acceptance {index} output changed but exits {:?}: {command_text}",
                        run.exit_code
                    ));
                }
            }
            plan::AcceptanceKind::Signatures(paths) => {
                // rung 5 re-reads like verify does, without executing
                // anything: identical shapes pass, anything else blocks
                let canonical = paths.join(", ");
                if !plan::signatures_current(&active.acceptance[index]) {
                    return Err(format!(
                        "no_signatures: acceptance {index} has no frozen shapes to compare to: {canonical}"
                    ));
                }
                let frozen = active.acceptance[index]
                    .shape
                    .as_ref()
                    .expect("gated above");
                let current = read_shapes(ctx, &frozen.files);
                let changed: Vec<&str> = frozen
                    .files
                    .iter()
                    .zip(current.iter())
                    .filter(|(frozen_file, (_, hash))| {
                        hash.as_deref() != Some(frozen_file.shape_hash.as_str())
                    })
                    .map(|(frozen_file, _)| frozen_file.path.as_str())
                    .collect();
                if !changed.is_empty() {
                    return Err(format!(
                        "signatures_changed: acceptance {index} declaration shapes changed: {}",
                        changed.join(", ")
                    ));
                }
            }
            plan::AcceptanceKind::Manual(text) => {
                // Passed rather than waived: it should not have been possible
                // to get here, so say so instead of letting it slide.
                return Err(format!(
                    "invalid_evidence: acceptance {index} is manual ({text}) and can only be                      waived by the user"
                ));
            }
            plan::AcceptanceKind::Text(_) => {
                validate_attached_records(
                    &root,
                    &active.id,
                    "acceptance",
                    &active.acceptance[index].evidence,
                )
                .map_err(|message| format!("acceptance {index}: {message}"))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_attached_records(
    root: &Path,
    plan_id: &str,
    step_id: &str,
    evidence: &[plan::EvidenceRef],
) -> Result<(), String> {
    if evidence.is_empty() {
        return Err(format!(
            "no_evidence: step {step_id} requires journal evidence"
        ));
    }
    let after_seq = if step_id == "acceptance" {
        None
    } else {
        crate::agent::journal::Journal::step_started_at(root, plan_id, step_id)
            .map_err(|e| format!("evidence_unreadable: {e:#}"))?
    };
    let mut valid = 0usize;
    let mut stale_epoch = 0usize;
    // Epoch of the step being validated. Acceptance-level checks have no
    // step; their refs were epoch-filtered when selected (unspent evidence).
    let step_epoch = if step_id == "acceptance" {
        None
    } else {
        plan::read_plan_file(root, plan_id).and_then(|plan| {
            plan.steps
                .iter()
                .find(|step| step.id == step_id)
                .map(|step| step.step_epoch)
        })
    };
    for reference in evidence {
        let record = crate::agent::journal::Journal::evidence(
            root,
            plan_id,
            if step_id == "acceptance" {
                None
            } else {
                Some(step_id)
            },
            reference,
            after_seq,
        )
        .map_err(|e| format!("evidence_unreadable: {e:#}"))?
        .ok_or_else(|| {
            format!(
                "invalid_evidence: journal record {} is not valid for this plan",
                reference.seq
            )
        })?;
        // Records from a stale step epoch (subagent work predating a reopen)
        // belong to the undone attempt, not the current one (§2.2.4).
        if let Some(epoch) = step_epoch
            && !crate::agent::journal::epoch_matches(&record, epoch)
        {
            stale_epoch += 1;
            continue;
        }
        // Kindless: what the step was *for* is the model's business. What
        // counts is that the records attest successful work — a failed exec
        // or errored diagnostics proves nothing (#198, §2.1.4). Diagnostics
        // count only with zero errors: the endpoint does not enforce that.
        let allowed = record.kind == "file_diff"
            || (record.kind == "tool_result"
                && record.fields.get("ok").and_then(Value::as_bool) == Some(true))
            || (record.kind == "diagnostics"
                && record.fields.get("errors").and_then(Value::as_u64) == Some(0));
        if allowed {
            valid += 1;
        }
    }
    if valid == 0 {
        if stale_epoch > 0 {
            return Err(format!(
                "stale_epoch: all {stale_epoch} evidence record(s) predate the last reopen of step {step_id}; do the work again under the current epoch"
            ));
        }
        return Err(format!(
            "wrong_evidence: evidence attests no successful work for step {step_id} — attach a successful exec, a file_diff, or zero-error diagnostics"
        ));
    }
    Ok(())
}

/// Rejections are a normal tool result the model can act on (§2.1.4).
pub(crate) fn rejection(r: plan::Rejection) -> Outcome {
    Outcome::err(
        json!({
            "ok": false,
            "code": r.code,
            "reason": r.reason,
            "hint": r.hint,
        })
        .to_string(),
    )
}

