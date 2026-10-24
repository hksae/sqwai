use super::loop_ask::{Grant, bash_call, mutating_tool_gate};
use super::*;

fn tainted_project(name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("sqwai-trust-gate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = "trust-sess".to_string();
    let mut journal = crate::agent::journal::Journal::open(&dir, &session).expect("journal opens");
    journal
        .append(
            "tool_result",
            serde_json::json!({"tool": "webfetch", "ok": true, "taint": "external"}),
        )
        .unwrap();
    (dir, session)
}

fn bash_call_parts(
    dir: &std::path::Path,
    session: &str,
) -> (
    ToolCallReq,
    tools::ToolCtx,
    tokio::sync::mpsc::Sender<AgentEvent>,
    tokio::sync::mpsc::Receiver<AgentEvent>,
    tokio::sync::mpsc::Sender<ControlMsg>,
    tokio::sync::mpsc::Receiver<ControlMsg>,
) {
    let (tx_agent, rx_ui) = tokio::sync::mpsc::channel(8);
    let (tx_ui, rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": "git push origin main"}),
    );
    let ctx = tools::ToolCtx::new(dir).in_session(session.to_string());
    (call, ctx, tx_agent, rx_ui, tx_ui, rx_agent)
}

/// Headless + external taint + egress: immediate denial, no prompt.
#[tokio::test]
async fn trust_gate_denies_egress_for_subagents_under_taint() {
    let (dir, session) = tainted_project("deny");
    let (call, mut ctx, tx_agent, _rx_ui, _tx_ui, mut rx_agent) = bash_call_parts(&dir, &session);
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        1,
        crate::config::ApprovalMode::Auto,
        &mut None,
        &mut None,
    )
    .await;
    assert!(!outcome.ok, "must refuse");
    assert!(
        outcome.output.contains("no user to confirm"),
        "{}",
        outcome.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Audit H1: a real spawned child gets a FRESH session — its own
/// journal has no taint records, so the parent's external exposure was
/// silently dropped and the trust gate never fired. The parent link
/// must carry the taint into the child's gate.
#[tokio::test]
async fn trust_gate_inherits_parent_taint_for_a_fresh_child_session() {
    let (dir, parent_session) = tainted_project("inherit");
    let child_session = "child-fresh".to_string();
    // the child's journal exists and is empty of taint, as after a
    // real spawn
    crate::agent::journal::Journal::open(&dir, &child_session).expect("journal opens");
    let (call, mut ctx, tx_agent, _rx_ui, _tx_ui, mut rx_agent) =
        bash_call_parts(&dir, &child_session);
    ctx.parent_session = Some(parent_session);
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        1,
        crate::config::ApprovalMode::Auto,
        &mut None,
        &mut None,
    )
    .await;
    assert!(
        !outcome.ok,
        "inherited taint must gate the child's egress: {}",
        outcome.output
    );
    assert!(
        outcome.output.contains("no user to confirm"),
        "{}",
        outcome.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Phase 0 probe — safe-bash checkpoint gap: a Safe-classified
/// mutating command on a clean tree. The hash-gate
/// (`needs_approval || tree_changed`) skips the pre-snapshot; the
/// probe pins that AND checks the other half — whether the mutation
/// stays revertible through the chain head (full-undo path). If both
/// hold, the gap is benign by design (the head predates the mutation,
/// so nothing is lost); if the revert fails, the gap is a hole.
#[tokio::test]
async fn safe_mutating_bash_on_clean_tree_skips_snapshot_but_stays_revertible() {
    use crate::config::ShadowStore;
    let dir = std::env::temp_dir().join(format!(
        "sqwai-probe-gap-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = "probe-gap";
    // clean tree, established head
    let head = crate::agent::checkpoints::snapshot_session(
        &dir,
        ShadowStore::Local,
        session,
        "probe base",
    )
    .unwrap()
    .expect("first snapshot commits");
    assert!(
        !crate::agent::checkpoints::tree_changed(&dir, ShadowStore::Local, session),
        "tree is clean against its head"
    );
    // Safe-classified, but mutates (verified Safe above this test)
    assert!(matches!(
        crate::agent::safety::classify("echo x > probe.txt"),
        crate::agent::safety::Verdict::Safe
    ));
    let call = crate::providers::ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": "echo x > probe.txt"}),
    );
    let (tx_agent, _rx_ui) = tokio::sync::mpsc::channel(8);
    let (_tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let mut ctx = tools::ToolCtx::new(&dir).in_session(session.to_string());
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut None,
        &mut None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert!(dir.join("probe.txt").exists(), "command mutated");
    // the gate skipped: no pre_bash snapshot journaled…
    assert!(
        !ctx.journal
            .iter()
            .any(|(_, label)| label.starts_with("bash ")),
        "no snapshot must be journaled for Safe-on-clean: {:?}",
        ctx.journal
    );
    // …but the mutation stays revertible through the head, which is
    // exactly what full /undo diffs against (unscoped fallback)
    let diff = crate::agent::checkpoints::changed_files(&dir, ShadowStore::Local, &head).unwrap();
    assert!(diff.iter().any(|p| p == "probe.txt"), "{diff:?}");
    let report = crate::agent::checkpoints::restore_paths_in(
        &dir,
        ShadowStore::Local,
        &head,
        &[crate::agent::checkpoints::Target {
            path: "probe.txt".to_string(),
            agent_hash: None,
        }],
    )
    .unwrap();
    assert!(!dir.join("probe.txt").exists(), "{report:?}");
    assert_eq!(report.deleted, vec!["probe.txt".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
}

/// Same command, clean session: no gate involved (git push then fails
/// on its own — no remote — proving the gate let it through).
#[tokio::test]
async fn trust_gate_stays_quiet_without_taint() {
    let dir = std::env::temp_dir().join(format!("sqwai-trust-clean-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (call, mut ctx, tx_agent, _rx_ui, _tx_ui, mut rx_agent) =
        bash_call_parts(&dir, "clean-sess");
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut None,
        &mut None,
    )
    .await;
    assert!(
        !outcome.output.contains("no user to confirm"),
        "{}",
        outcome.output
    );
    assert!(
        !outcome.output.contains("external content"),
        "{}",
        outcome.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Tainted main session: the approval dialog carries the trust reason,
/// and a denial stops the command.
#[tokio::test]
async fn trust_gate_prompts_with_reason_and_honors_deny() {
    let (dir, session) = tainted_project("prompt");
    let (call, mut ctx, tx_agent, mut rx_ui, tx_ui, mut rx_agent) = bash_call_parts(&dir, &session);
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let mut no_journal: Option<crate::agent::journal::Journal> = None;
    let mut no_target: Option<std::path::PathBuf> = None;
    let future = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut no_journal,
        &mut no_target,
    );
    tokio::pin!(future);
    let outcome = loop {
        tokio::select! {
            out = &mut future => break out,
            ev = rx_ui.recv() => {
                if let Some(AgentEvent::Approval { id, reason, .. }) = ev {
                    assert!(
                        reason.contains("external content"),
                        "trust reason missing: {reason}"
                    );
                    tx_ui
                        .send(ControlMsg::ApprovalAnswer {
                            id,
                            decision: ApprovalDecision::Deny,
                        })
                        .await
                        .unwrap();
                }
            }
        }
    };
    assert!(!outcome.ok, "denied command must fail");
    assert!(
        outcome.output.contains("denied by user"),
        "{}",
        outcome.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// one dialog per (session, egress kind). After the user answers for
/// a push, the next push in the same session runs without asking, and the
/// answer is in the journal — so it survives a resume.
#[tokio::test]
async fn an_approved_egress_kind_does_not_ask_again() {
    let (dir, session) = tainted_project("ack");
    let (call, mut ctx, tx_agent, mut rx_ui, tx_ui, mut rx_agent) = bash_call_parts(&dir, &session);
    let mut journal =
        Some(crate::agent::journal::Journal::open(&dir, &session).expect("journal opens"));
    let mut no_target: Option<std::path::PathBuf> = None;
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    // first push: the dialog runs and the user answers once. Each run is
    // scoped so the future's borrows end before the next call reuses the
    // same parts.
    let asked = {
        let future = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut journal,
        &mut no_target,
    );
        let mut asked = false;
        tokio::pin!(future);
        let _ = loop {
            tokio::select! {
                out = &mut future => break out,
                ev = rx_ui.recv() => {
                    if let Some(AgentEvent::Approval { id, .. }) = ev {
                        asked = true;
                        tx_ui
                            .send(ControlMsg::ApprovalAnswer {
                                id,
                                decision: ApprovalDecision::RunOnce,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        };
        asked
    };
    assert!(asked, "the first tainted push must ask");
    assert_eq!(
        crate::agent::trust::acked_egress(&dir, &session),
        vec!["git push".to_string()],
        "the answer must be journaled"
    );
    // second push, same kind, different spelling: no dialog
    let second = crate::providers::ToolCallReq::new(
        "c2",
        "bash",
        serde_json::json!({"command": "git push origin HEAD"}),
    );
    let asked_again = {
        let future = bash_call(
        &second,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut journal,
        &mut no_target,
    );
        let mut asked_again = false;
        tokio::pin!(future);
        let _ = loop {
            tokio::select! {
                out = &mut future => break out,
                ev = rx_ui.recv() => {
                    if let Some(AgentEvent::Approval { id, .. }) = ev {
                        asked_again = true;
                        tx_ui
                            .send(ControlMsg::ApprovalAnswer {
                                id,
                                decision: ApprovalDecision::Deny,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        };
        asked_again
    };
    assert!(!asked_again, "the same egress kind must not ask twice");
    std::fs::remove_dir_all(&dir).ok();
}

/// #191: "always allow" grants the shape the user saw, not the exact bytes of
/// one command. Same head, same reason, no new flag ⇒ pass. A flag that was
/// never in front of them ⇒ ask again.
#[test]
fn a_grant_follows_the_shape_of_the_risk() {
    let granted = Grant::from_command("git commit -m first", "git history rewrite");
    assert!(
        granted.covers("git commit -m second", "git history rewrite"),
        "a new message is the same shape the user approved"
    );
    assert!(!granted.covers("git push origin main", "git history rewrite"));
    assert!(
        !granted.covers("git commit --no-verify -m second", "git history rewrite"),
        "a flag the user never saw is not granted"
    );
    assert!(!granted.covers("git commit -m second", "force push"));

    let wide = Grant::from_command("git commit -m x --amend", "r");
    assert!(
        wide.covers("git commit -m y", "r"),
        "dropping an approved flag cannot add risk"
    );

    let rm = Grant::from_command("rm notes.txt", "recursive delete");
    assert!(
        !rm.covers("rm -rf build", "recursive delete"),
        "the classic widening stays behind a dialog"
    );

    // Target-scoped destructive commands: `rm -rf target/debug` covers the
    // same target, but NEVER covers `rm -rf src` or a chained `&& sudo reboot`
    let rm_target = Grant::from_command("rm -rf target/debug", "recursive delete");
    assert!(rm_target.covers("rm -rf target/debug", "recursive delete"));
    assert!(
        !rm_target.covers("rm -rf src", "recursive delete"),
        "destructive grant must stay pinned to the approved target path"
    );
    assert!(
        !rm_target.covers("rm -rf target/debug && rm -rf src", "recursive delete"),
        "compound command with an unapproved second target must not ride on the first"
    );

    // Exec wrappers (`sudo`, `xargs`, `find`) are exact-only, never wildcarded
    let sudo_grant = Grant::from_command("sudo rm /tmp/a", "privilege/system control");
    assert!(sudo_grant.covers("sudo rm /tmp/a", "privilege/system control"));
    assert!(
        !sudo_grant.covers("sudo rm /etc/passwd", "privilege/system control"),
        "sudo grant must never wildcard inner arguments"
    );
}

/// Ask mode confirms every command, even one the classifier calls safe.
#[tokio::test]
async fn ask_mode_prompts_even_for_a_safe_command() {
    let dir = std::env::temp_dir().join(format!("sqwai-ask-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = "ask-sess".to_string();
    let (tx_agent, mut rx_ui) = tokio::sync::mpsc::channel(8);
    let (tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new("c1", "bash", serde_json::json!({"command": "echo hi"}));
    let mut ctx = tools::ToolCtx::new(&dir).in_session(session.clone());
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let mut no_journal: Option<crate::agent::journal::Journal> = None;
    let mut no_target: Option<std::path::PathBuf> = None;
    let future = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Ask,
        &mut no_journal,
        &mut no_target,
    );
    // the block scope drops the pinned future (and its borrows) before
    // the assertions look at `always_allow`
    let asked = {
        tokio::pin!(future);
        let mut asked = false;
        loop {
            tokio::select! {
                out = &mut future => {
                    assert!(out.ok, "echo hi must run once answered: {}", out.output);
                    break;
                }
                ev = rx_ui.recv() => {
                    if let Some(AgentEvent::Approval { id, subject, .. }) = ev {
                        asked = true;
                        assert_eq!(subject, "echo hi");
                        tx_ui
                            .send(ControlMsg::ApprovalAnswer {
                                id,
                                decision: ApprovalDecision::RunOnce,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        }
        asked
    };
    assert!(asked, "a safe command still asks in ask mode");
    assert!(always_allow.is_empty(), "run once earns no grant");
    std::fs::remove_dir_all(&dir).ok();
}

/// Full access runs a classifier-flagged command with no dialog at all —
/// and the checkpoint insurance stays armed (the snapshot is what the
/// dangerous ones lose when the prompt goes).
#[tokio::test]
async fn full_mode_runs_the_dangerous_command_without_asking() {
    let dir = std::env::temp_dir().join(format!("sqwai-full-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("victim")).unwrap();
    std::fs::write(dir.join("victim/f.txt"), "x").unwrap();
    let session = "full-sess".to_string();
    let (tx_agent, mut rx_ui) = tokio::sync::mpsc::channel(8);
    let (_tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": format!("rm -rf \"{}\"", dir.join("victim").display())}),
    );
    let mut ctx = tools::ToolCtx::new(&dir).in_session(session);
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let mut no_journal: Option<crate::agent::journal::Journal> = None;
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Full,
        &mut no_journal,
        &mut None,
    )
    .await;
    assert!(outcome.ok, "full mode runs it: {}", outcome.output);
    assert!(
        !dir.join("victim").exists(),
        "the command really executed"
    );
    while let Ok(ev) = rx_ui.try_recv() {
        assert!(
            !matches!(ev, AgentEvent::Approval { .. }),
            "full mode must not ask"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Full access lifts prompts only: the deny-side gates — blocked patterns
/// and host-owned paths — keep refusing exactly as before.
#[tokio::test]
async fn full_mode_keeps_the_hard_gates() {
    let dir = std::env::temp_dir().join(format!("sqwai-full-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = "full-gate-sess".to_string();
    let (tx_agent, _rx_ui) = tokio::sync::mpsc::channel(8);
    let (_tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": "rm -rf .sqwai"}),
    );
    let mut ctx = tools::ToolCtx::new(&dir).in_session(session);
    let blocked = vec![("no-forever".to_string(), regex::Regex::new("forever").unwrap())];
    // blocked pattern wins even in full
    let mut next_id = 0u64;
    let mut no_journal: Option<crate::agent::journal::Journal> = None;
    let call_hit = ToolCallReq::new(
        "c0",
        "bash",
        serde_json::json!({"command": "echo forever"}),
    );
    let outcome = bash_call(
        &call_hit,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut Vec::new(),
        &blocked,
        &mut next_id,
        0,
        crate::config::ApprovalMode::Full,
        &mut no_journal,
        &mut None,
    )
    .await;
    assert!(!outcome.ok, "blocked_patterns hold in full mode");
    assert!(outcome.output.contains("no-forever"));
    // host-owned path wins even in full
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut Vec::new(),
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Full,
        &mut no_journal,
        &mut None,
    )
    .await;
    assert!(
        !outcome.ok && outcome.output.contains("protected"),
        "the .sqwai/ paths stay protected in full mode: {}",
        outcome.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The workspace boundary asks, it does not refuse: a bash write beyond
/// the project root opens the approval dialog with the outside reason.
#[tokio::test]
async fn outside_bash_write_asks_in_auto() {
    let base = std::env::temp_dir().join(format!("sqwai-outside-ask-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let outside = base.join("out.txt");
    let (tx_agent, mut rx_ui) = tokio::sync::mpsc::channel(8);
    let (tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": format!("echo hi > \"{}\"", outside.display())}),
    );
    let mut ctx = tools::ToolCtx::new(&root).in_session("outside-sess".to_string());
    let mut always_allow = Vec::new();
    let mut next_id = 0u64;
    let mut no_journal: Option<crate::agent::journal::Journal> = None;
    let mut no_target: Option<std::path::PathBuf> = None;
    let future = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        crate::config::ApprovalMode::Auto,
        &mut no_journal,
        &mut no_target,
    );
    let asked = {
        tokio::pin!(future);
        let mut asked = false;
        loop {
            tokio::select! {
                out = &mut future => {
                    assert!(out.ok, "answered once, the write runs: {}", out.output);
                    break;
                }
                ev = rx_ui.recv() => {
                    if let Some(AgentEvent::Approval { id, reason, .. }) = ev {
                        asked = true;
                        assert_eq!(reason, "writes outside the project directory");
                        tx_ui
                            .send(ControlMsg::ApprovalAnswer {
                                id,
                                decision: ApprovalDecision::RunOnce,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        }
        asked
    };
    assert!(asked, "an outside write must ask, not refuse");
    assert!(outside.exists(), "the approved write really landed");
    let _ = std::fs::remove_dir_all(&base);
}

/// Full access lifts the workspace prompt like every other prompt: the
/// outside write runs with no dialog at all.
#[tokio::test]
async fn outside_bash_write_runs_silently_in_full() {
    let base = std::env::temp_dir().join(format!("sqwai-outside-full-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let outside = base.join("out.txt");
    let (tx_agent, mut rx_ui) = tokio::sync::mpsc::channel(8);
    let (_tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "bash",
        serde_json::json!({"command": format!("echo hi > \"{}\"", outside.display())}),
    );
    let mut ctx = tools::ToolCtx::new(&root).in_session("outside-full-sess".to_string());
    let outcome = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut Vec::new(),
        &[],
        &mut 0u64,
        0,
        crate::config::ApprovalMode::Full,
        &mut None,
        &mut None,
    )
    .await;
    assert!(outcome.ok, "full access writes outside: {}", outcome.output);
    assert!(outside.exists());
    while let Ok(ev) = rx_ui.try_recv() {
        assert!(!matches!(ev, AgentEvent::Approval { .. }), "no dialog in full");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The file-tool side of the same boundary: the gate asks, denial is a
/// normal refused outcome, approval arms the one-shot jail permit — and
/// full access arms it with no dialog.
#[tokio::test]
async fn outside_file_write_gate_denies_and_permits() {
    let base = std::env::temp_dir().join(format!("sqwai-outside-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let (tx_agent, mut rx_ui) = tokio::sync::mpsc::channel(8);
    let (tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
    let call = ToolCallReq::new(
        "c1",
        "write",
        serde_json::json!({"file_path": "../out.txt", "content": "x"}),
    );
    // deny: the call is refused, the permit stays down
    let ev_task = tokio::spawn(async move {
        loop {
            match rx_ui.recv().await {
                Some(AgentEvent::Approval { id, subject, reason, .. }) => {
                    assert!(subject.starts_with("write "), "{subject}");
                    assert_eq!(reason, "writes outside the project directory");
                    tx_ui
                        .send(ControlMsg::ApprovalAnswer {
                            id,
                            decision: ApprovalDecision::Deny,
                        })
                        .await
                        .unwrap();
                }
                Some(_) => continue,
                None => break,
            }
        }
    });
    let mut ctx = tools::ToolCtx::new(&root).in_session("gate-sess".to_string());
    let refused = mutating_tool_gate(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut 0u64,
        crate::config::ApprovalMode::Auto,
        &mut Vec::new(),
    )
    .await
    .expect("denied");
    assert!(!refused.ok);
    assert!(refused.output.contains("denied by user"));
    assert!(
        ctx.outside_roots.is_empty(),
        "denial opens no directory"
    );
    ev_task.abort();

    // full access: no dialog, the call resolves on its own
    let (tx_agent2, rx_ui2) = tokio::sync::mpsc::channel(8);
    let (_tx_ui2, mut rx_agent2) = tokio::sync::mpsc::channel(8);
    std::mem::drop(rx_ui2);
    let mut ctx2 = tools::ToolCtx::new(&root).in_session("gate-sess".to_string());
    ctx2.outside_all = true;
    let gate = mutating_tool_gate(
        &call,
        &mut ctx2,
        &tx_agent2,
        &mut rx_agent2,
        &mut 0u64,
        crate::config::ApprovalMode::Full,
        &mut Vec::new(),
    )
    .await;
    assert!(gate.is_none(), "full access runs the outside write");
    let _ = std::fs::remove_dir_all(&base);
}
