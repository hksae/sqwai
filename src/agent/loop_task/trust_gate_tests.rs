use super::loop_ask::{Grant, bash_call};
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
    let future = bash_call(
        &call,
        &mut ctx,
        &tx_agent,
        &mut rx_agent,
        &mut always_allow,
        &[],
        &mut next_id,
        0,
        &mut no_journal,
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
            &mut journal,
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
            &mut journal,
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

/// #185: which files get re-linted comes from the call's own diff report, not
/// from a list of tool names. A multi-file `patch` reaches the LSP with every
/// file it touched, a single-file tool with its one, and a tool that reports no
/// path (`bash`) reports none — which is not the same as "nothing changed".
#[test]
fn lint_targets_come_from_the_calls_diff_report() {
    use crate::agent::tools::{FileDiff, Outcome};
    let root = std::path::Path::new(if cfg!(windows) { r"C:\proj" } else { "/proj" });
    let diff = |path: &str| FileDiff {
        path: path.into(),
        added: 1,
        removed: 0,
        hash_before: None,
        hash_after: "h".into(),
        mode: "file".into(),
        checkpoint: None,
        blob_before: None,
        blob_after: None,
    };

    let mut single = Outcome::ok("written");
    single.file_diff = Some(diff("src/a.rs"));
    assert_eq!(
        super::lint_targets(&single, root),
        vec![root.join("src/a.rs")]
    );

    let mut multi = Outcome::ok("patch applied");
    multi.file_diff = Some(diff("src/a.rs"));
    multi.file_diffs = vec![diff("src/a.rs"), diff(r"src\b.rs"), diff("c.txt")];
    assert_eq!(
        super::lint_targets(&multi, root),
        vec![
            root.join("src/a.rs"),
            root.join("src/b.rs"),
            root.join("c.txt"),
        ],
        "every reported file, in order, without duplicates"
    );

    let bash = Outcome::ok("no output");
    assert!(
        super::lint_targets(&bash, root).is_empty(),
        "a tool with no per-call path reports none"
    );
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
