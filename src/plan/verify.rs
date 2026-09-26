use super::{
    Acceptance, AcceptanceKind, AcceptanceStatus, Applied, Baseline, CheckInput, EvidenceRef,
    Plan, PlanStatus, Receipt, Rejection, ShapeFreeze, Snapshot, StepStatus, ValidationStatus,
};
use super::ops::{accept, reject};
use std::path::Path;


/// Manifests that every state digest covers in addition to the traversed
/// paths: a dependency or toolchain move invalidates checks even when no
/// tracked file changed.
const STATE_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
];

/// Canonical path form for digest inputs and receipt invalidation. Refs are
/// declared with forward slashes; diffs may arrive OS-native.
pub(crate) fn norm_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_string()
}

/// Sorted union of declared step ref paths: the digest input set (§2.1.4,
/// decision 2+3). Deterministic for a fixed plan, so equal digests mean
/// nothing relevant moved.
pub fn digest_paths(plan: &Plan) -> Vec<String> {
    let mut paths = std::collections::BTreeSet::new();
    for step in &plan.steps {
        for r in &step.refs {
            paths.insert(norm_path(&r.path));
        }
    }
    paths.into_iter().collect()
}

/// blake3 state digest over traversed file contents (missing files hash as
/// a tombstone, so creation/deletion moves the digest), manifest contents,
/// and the check text. Computed before AND after a check run: only equal
/// halves make a passing receipt.
pub fn state_digest(root: &Path, paths: &[String], command: &str) -> String {
    let mut h = blake3::Hasher::new();
    for path in paths {
        h.update(path.as_bytes());
        h.update(b"\0");
        if let Ok(bytes) = std::fs::read(root.join(path)) {
            h.update(&bytes);
        } else {
            h.update(b"<missing>");
        }
        h.update(b"\0");
    }
    for manifest in STATE_MANIFESTS {
        if let Ok(bytes) = std::fs::read(root.join(manifest)) {
            h.update(manifest.as_bytes());
            h.update(b"\0");
            h.update(&bytes);
            h.update(b"\0");
        }
    }
    h.update(command.as_bytes());
    h.finalize().to_hex().to_string()
}

/// Identity of a check definition: same command text, same check.
pub fn check_definition_hash(command: &str) -> String {
    blake3::hash(command.as_bytes()).to_hex().to_string()
}

/// §12.12: does this item carry proof that its check *discriminates*?
///
/// True only for a `cmd:` item whose baseline was taken against the check
/// definition it still has — rewriting the command makes a new check, and the
/// old failing run stops being evidence about it. Manual items and free text
/// can never be proven this way, which is why they settle on other terms.
pub fn proven_failing(item: &Acceptance) -> bool {
    let AcceptanceKind::Command(command) = item.kind() else {
        return false;
    };
    item.baseline
        .as_ref()
        .is_some_and(|b| b.check_definition_hash == check_definition_hash(command))
}

/// §12.12, judge ladder rung 4: does this item carry frozen behavior to
/// compare against?
///
/// True only for a `snapshot:` item whose output was frozen against the
/// check definition it still has — rewriting the command makes a new check,
/// and the old output stops applying to it.
pub fn snapshot_current(item: &Acceptance) -> bool {
    let AcceptanceKind::Snapshot(command) = item.kind() else {
        return false;
    };
    item.snapshot
        .as_ref()
        .is_some_and(|s| s.check_definition_hash == check_definition_hash(command))
}

/// Rung 3: same frozen-output rule as [`snapshot_current`], for a
/// `differential:` item. The freeze is shared (one [`Snapshot`] record);
/// only the verdict is inverted — changed output settles instead of
/// identical output.
pub fn differential_current(item: &Acceptance) -> bool {
    let AcceptanceKind::Differential(command) = item.kind() else {
        return false;
    };
    item.snapshot
        .as_ref()
        .is_some_and(|s| s.check_definition_hash == check_definition_hash(command))
}

/// Judge ladder rungs (§12.12), ordered by trust per unit of cost — the
/// declaration order IS the trust order. The host walks down and stops at
/// the first rung that applies; only classification ships so far (no
/// synthesis, no gating). No rung 6: no acceptance kind was ever defined
/// for round-trip, and rung synthesis was never specified — both dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// 1–2: an existing test, or a repro written for the change. The host
    /// cannot tell those apart from text alone, so they share the rung.
    Test,
    /// 3: `differential:`
    Differential,
    /// 4: `snapshot:`
    Snapshot,
    /// 5: `signatures:`
    Structural,
    /// 7: builds, type checks, lints, `--dry-run`s
    Build,
    /// 8: any other host-run command (a fixture run by another name)
    Fixture,
}

impl Rung {
    pub fn number(self) -> u8 {
        match self {
            Self::Test => 1,
            Self::Differential => 3,
            Self::Snapshot => 4,
            Self::Structural => 5,
            Self::Build => 7,
            Self::Fixture => 8,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Differential => "differential",
            Self::Snapshot => "snapshot",
            Self::Structural => "structural",
            Self::Build => "build",
            Self::Fixture => "fixture",
        }
    }
}

/// Which ladder rung an acceptance item engages, if any. Kinds that name
/// their rung (`differential:`, `snapshot:`, `signatures:`) map exactly;
/// `cmd:` maps by a text heuristic — informational only, never a gate —
/// and `manual:`/free text engage no rung at all.
pub fn ladder_rung(item: &Acceptance) -> Option<Rung> {
    match item.kind() {
        AcceptanceKind::Differential(_) => Some(Rung::Differential),
        AcceptanceKind::Snapshot(_) => Some(Rung::Snapshot),
        AcceptanceKind::Signatures(_) => Some(Rung::Structural),
        AcceptanceKind::Command(command) => Some(classify_command_rung(command)),
        AcceptanceKind::Manual(_) | AcceptanceKind::Text(_) => None,
    }
}

/// Heuristic half of [`ladder_rung`]: test-shaped text outranks
/// build-shaped text, everything else reads as a fixture run. Substring
/// matching overmatches (`latest` reads as a test) — accepted, because
/// the rung informs arbitration order instead of deciding anything.
fn classify_command_rung(command: &str) -> Rung {
    let lower = command.to_lowercase();
    if lower.contains("test") || lower.contains("spec") {
        Rung::Test
    } else if [
        "check", "build", "lint", "clippy", "mypy", "pyright", "tsc", "dry-run", "dryrun",
        "validate", "schema", "compile", "audit",
    ]
    .iter()
    .any(|token| lower.contains(token))
    {
        Rung::Build
    } else {
        Rung::Fixture
    }
}

/// The walk itself: the highest-trust rung the plan engages, if any.
pub fn ladder_top(plan: &Plan) -> Option<Rung> {
    plan.acceptance.iter().filter_map(ladder_rung).min()
}

/// One trailing line for the plan-create/accept result: where on the
/// ladder this plan stands.
pub fn ladder_note(plan: &Plan) -> String {
    match ladder_top(plan) {
        Some(rung) => format!(
            "\nladder: rung {} {} — highest-trust executable acceptance",
            rung.number(),
            rung.name()
        ),
        None => "\nladder: no executable rung — manual/text only".to_string(),
    }
}

/// Rung 5: does this item carry frozen declaration shapes to compare
/// against? True only for a `signatures:` item whose shapes were frozen
/// against the file set it still names — renaming the set makes a new
/// check, and the old shapes stop applying to it.
pub fn signatures_current(item: &Acceptance) -> bool {
    let AcceptanceKind::Signatures(paths) = item.kind() else {
        return false;
    };
    let text = paths.join(", ");
    item.shape
        .as_ref()
        .is_some_and(|s| s.check_definition_hash == check_definition_hash(&text))
}

/// Host-only: attach the baselines captured at plan time (§12.12). The vector
/// is positional against the acceptance list; a missing or short vector leaves
/// the remaining items without a baseline, which is a state they can be shown
/// in but never settled from.
pub fn set_baselines(plan: &mut Plan, baselines: Vec<Option<Baseline>>) {
    for (item, baseline) in plan.acceptance.iter_mut().zip(baselines) {
        item.baseline = baseline;
    }
}

/// Host-only: attach the snapshots frozen at plan time (§12.12, rung 4).
/// Positional like [`set_baselines`]; items past the vector stay unfrozen.
pub fn set_snapshots(plan: &mut Plan, snapshots: Vec<Option<Snapshot>>) {
    for (item, snapshot) in plan.acceptance.iter_mut().zip(snapshots) {
        item.snapshot = snapshot;
    }
}

/// Host-only: attach the declaration shapes frozen at plan time (rung 5).
/// Positional like [`set_baselines`]; items past the vector stay unfrozen.
pub fn set_shapes(plan: &mut Plan, shapes: Vec<Option<ShapeFreeze>>) {
    for (item, shape) in plan.acceptance.iter_mut().zip(shapes) {
        item.shape = shape;
    }
}

/// Host-only: attach frozen check inputs, positional like [`set_baselines`].
pub fn set_inputs(plan: &mut Plan, inputs: Vec<Vec<CheckInput>>) {
    for (item, item_inputs) in plan.acceptance.iter_mut().zip(inputs) {
        item.inputs = item_inputs;
    }
}

/// Directories whose whole subtrees are check inputs by convention.
const CHECK_INPUT_DIRS: &[&str] = &["tests", "test", "spec", "specs", "fixtures", "snapshots"];

/// Walked but never frozen (or even descended into): VCS, host state,
/// build outputs.
const CHECK_INPUT_SKIPS: &[&str] = &[".git", ".sqwai", "target", "node_modules"];

/// Caps: freezing is plan-time overhead on every create.
const CHECK_INPUT_MAX_FILES: usize = 500;
const CHECK_INPUT_MAX_BYTES: u64 = 2_000_000;

/// Hash the check inputs that exist right now: test and fixture files by
/// conventional layout. Sorted for determinism. Only pre-existing files
/// are listed — new test files are always allowed (rung 2 lives on that).
///
/// Known gap, documented not hidden: Rust unit tests live inside `src/`
/// (`#[cfg(test)]`), which no glob can isolate from the code under test.
/// Those are covered by the confirm-gate on write, not by this freeze.
pub fn freeze_check_inputs(root: &Path) -> Vec<CheckInput> {
    let mut paths = Vec::new();
    collect_check_inputs(root, root, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .take(CHECK_INPUT_MAX_FILES)
        .filter_map(|path| {
            let bytes = std::fs::read(root.join(&path)).ok()?;
            if bytes.len() as u64 > CHECK_INPUT_MAX_BYTES {
                return None;
            }
            Some(CheckInput {
                path,
                hash: blake3::hash(&bytes).to_hex().to_string(),
            })
        })
        .collect()
}

fn collect_check_inputs(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if CHECK_INPUT_SKIPS.contains(&name.as_str()) {
                continue;
            }
            collect_check_inputs(root, &path, out);
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if is_check_input(&rel, &name) {
            out.push(rel);
        }
    }
}

fn is_check_input(rel: &str, name: &str) -> bool {
    if rel.split('/').any(|comp| CHECK_INPUT_DIRS.contains(&comp)) {
        return true;
    }
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.starts_with("test_") || stem.ends_with("_test") || name.ends_with(".snap")
}

/// Re-hash frozen inputs; returns the paths that changed or vanished.
/// Empty means the check still runs against what was frozen.
pub fn changed_check_inputs(root: &Path, inputs: &[CheckInput]) -> Vec<String> {
    inputs
        .iter()
        .filter(|input| {
            match std::fs::read(root.join(&input.path)) {
                Ok(bytes) => blake3::hash(&bytes).to_hex().to_string() != input.hash,
                Err(_) => true,
            }
        })
        .map(|input| input.path.clone())
        .collect()
}

/// Paths no model write may touch without the user taking responsibility:
/// frozen inputs of every non-waived acceptance item. Waiving unfreezes.
pub fn frozen_input_paths(plan: &Plan) -> Vec<String> {
    plan.acceptance
        .iter()
        .filter(|item| item.status != AcceptanceStatus::Waived)
        .flat_map(|item| item.inputs.iter().map(|input| input.path.clone()))
        .collect()
}

/// Mark an acceptance item verified on the host's terms.
///
/// `evidence` is what the host is prepared to stand behind for *this* item:
/// the scoped journal references for a `Text` item, and empty for a `Command`
/// item, whose verification is the host having just run the command (and
/// running it again at `complete`).
///
/// This used to take no evidence at all and dig out `steps.iter().find(kind ==
/// Verify && !evidence.is_empty())` — the first verify step with anything
/// attached, regardless of which acceptance item was being verified. One
/// successful command let every acceptance item pass in turn, reusing the same
/// record, so "complete requires every acceptance criterion verified" meant
/// "something succeeded once".
pub fn verify_acceptance(
    plan: &mut Plan,
    index: usize,
    evidence: Vec<EvidenceRef>,
    supplied_evidence: bool,
    receipt: Option<Receipt>,
) -> Result<Applied, Rejection> {
    if index >= plan.acceptance.len() {
        return reject(
            plan,
            "unknown_acceptance",
            format!("no acceptance item {index}"),
            "call plan show to see the acceptance list",
        );
    }
    match plan.acceptance[index].kind() {
        AcceptanceKind::Manual(text) => {
            return reject(
                plan,
                "manual_acceptance",
                format!("acceptance {index} is manual: {text}"),
                "no command settles this one; ask the user to waive it with \
                 /plan waive",
            );
        }
        AcceptanceKind::Command(_) => {
            // the host ran it before calling this; nothing to point at, and
            // `complete` runs it again rather than trusting an old record.
            // Whether the check has ever been *able* to fail is a rule about
            // what the host may accept, so it lives at the host boundary
            // (`tools::verify_acceptance`) and not here: replay restores
            // commits that were already accepted, and must not be re-judged.
        }
        AcceptanceKind::Snapshot(_) | AcceptanceKind::Differential(_) => {
            // same contract as a command: the host ran it, froze the output
            // before, and compared just now. Whether anything was frozen is
            // judged at the host boundary for the same replay reason.
        }
        AcceptanceKind::Signatures(_) => {
            // same contract again: the host read the shapes before and
            // re-read them just now; the freeze gate lives at the host
            // boundary so replay never re-judges accepted commits.
        }
        AcceptanceKind::Text(text) => {
            // Untyped acceptance cannot be created anymore; files written
            // before the gate still load, but nothing settles them.
            return reject(
                plan,
                "untyped_acceptance",
                format!("acceptance {index} is free text: {text}"),
                "rewrite it as cmd: (pass/fail), snapshot: (frozen output), \
                 differential: (changed output), signatures: (file shapes), \
                 or manual: (the user checks it by hand)",
            );
        }
    }
    let item = &mut plan.acceptance[index];
    item.status = AcceptanceStatus::Passed;
    item.evidence = evidence;
    // passed validation rides with the check that earned it: a host-run
    // command attaches its interval receipt, evidence-backed items record
    // the pass itself. Replay of pre-receipt commits passes `None` and
    // keeps the legacy shape (status without validation).
    if let Some(receipt) = receipt {
        item.validation.status = ValidationStatus::Passed;
        item.validation.receipts.push(receipt);
    }
    let message = if supplied_evidence {
        format!("acceptance {index} verified (model evidence ignored; host evidence used)")
    } else {
        format!("acceptance {index} verified")
    };
    accept(plan, message)
}

/// An acceptance item verified under previous rules: `Passed` status with
/// no validation block at all. New verifies always set validation, and
/// replay heals it for journaled commits — this clause is only for plan
/// files that predate receipts.
pub(crate) fn legacy_passed(item: &Acceptance) -> bool {
    item.status == AcceptanceStatus::Passed
        && item.validation.status == ValidationStatus::Pending
        && item.validation.receipts.is_empty()
}

pub(crate) fn complete(plan: &mut Plan) -> Result<Applied, Rejection> {
    let pending: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                StepStatus::Pending
                    | StepStatus::InProgress
                    | StepStatus::Blocked
                    | StepStatus::Reopened
            )
        })
        .map(|s| s.id.clone())
        .collect();
    if !pending.is_empty() {
        return reject(
            plan,
            "steps_open",
            format!("steps still open: {}", pending.join(", ")),
            "finish, unblock or cancel them first",
        );
    }
    // §2.1.4: every item needs validation passed|waived. Stale is its own
    // rejection (state moved under a recorded check: re-verify, don't
    // complete on it); legacy passes predate receipts and count as-is.
    let pending: Vec<usize> = plan
        .acceptance
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            // stale items report through the stale error below, not here
            a.validation.status != ValidationStatus::Stale
                && !matches!(
                    a.validation.status,
                    ValidationStatus::Passed | ValidationStatus::Waived
                )
                && !legacy_passed(a)
        })
        .map(|(i, _)| i)
        .collect();
    let stale: Vec<usize> = plan
        .acceptance
        .iter()
        .enumerate()
        .filter(|(_, a)| a.validation.status == ValidationStatus::Stale)
        .map(|(i, _)| i)
        .collect();
    if !pending.is_empty() {
        return reject(
            plan,
            "acceptance_pending",
            format!(
                "acceptance items still pending: {}",
                pending
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "verify them, or have the user waive them with /plan waive. Pending acceptance items without cmd:/manual: prefix require user waiver (/plan waive <index>) or conversion to steps with host evidence.",
        );
    }
    if !stale.is_empty() {
        return reject(
            plan,
            "acceptance_stale",
            format!(
                "acceptance items went stale after verification: {}",
                stale
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "tracked files changed under a recorded check; re-verify the items, then complete",
        );
    }
    plan.status = PlanStatus::Completed;
    plan.revision += 1;
    plan.rejections_in_a_row = 0;
    Ok(Applied::Completed)
}

pub(crate) fn next_id(plan: &Plan) -> String {
    let max = plan
        .steps
        .iter()
        .filter_map(|s| s.id.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    (max + 1).to_string()
}

