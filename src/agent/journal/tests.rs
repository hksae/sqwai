use super::*;
fn root() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

#[test]
fn resync_advances_past_externally_appended_records() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    assert_eq!(journal.append("a", json!({})).unwrap(), 1);
    // a short-lived handle (plan::commit) wrote behind our back
    let mut other = Journal::open(&root, "session").unwrap();
    assert_eq!(other.append("b", json!({})).unwrap(), 2);
    journal.resync().expect("resync");
    assert_eq!(journal.append("c", json!({})).unwrap(), 3);
}

/// #189: a resync failure is loud and keeps the counter — never a
/// silent stale reuse of sequence numbers.
#[test]
fn resync_failure_returns_an_error() {
    use std::io::Write;
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    assert_eq!(journal.append("a", json!({})).unwrap(), 1);
    // corrupt the tail with a non-record line
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(journal.path())
        .unwrap();
    writeln!(f, "{{not json}}").unwrap();
    drop(f);
    assert!(journal.resync().is_err(), "corrupt tail must fail loudly");
    assert_eq!(journal.next_seq(), 2, "counter untouched on failure");
}

#[test]
fn appends_monotonic_records_with_host_fields() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
    assert_eq!(
        journal
            .append("tool_call", json!({"tool": "read"}))
            .unwrap(),
        1
    );
    assert_eq!(
        journal.append("tool_result", json!({"ok": true})).unwrap(),
        2
    );
    let text = fs::read_to_string(journal.path()).unwrap();
    let records: Vec<Record> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].seq, 1);
    assert_eq!(records[1].kind, "tool_result");
    assert_eq!(records[0].step.as_deref(), Some("2"));
    fs::remove_dir_all(root).ok();
}

/// Several handles, one file (loop writer + fresh opens in plan commits,
/// receipts, undo): interleaved appends must stay monotonic, or a later
/// `open`/`resync` bails with "142 after 143" and plan ops start failing.
#[test]
fn interleaved_handles_stay_monotonic() {
    let root = root();
    let mut main = Journal::open(&root, "session").unwrap();
    assert_eq!(main.append("tool_call", json!({})).unwrap(), 1);
    // another handle advances the file twice behind main's back
    // (e.g. receipt append + plan commit during verify)
    let mut other = Journal::open(&root, "session").unwrap();
    assert_eq!(other.append("note", json!({})).unwrap(), 2);
    assert_eq!(other.append("plan", json!({})).unwrap(), 3);
    // main's counter is stale (2) — the append must heal, not reuse
    assert_eq!(main.append("tool_result", json!({})).unwrap(), 4);
    let text = fs::read_to_string(main.path()).unwrap();
    let seqs: Vec<u64> = text
        .lines()
        .map(|l| serde_json::from_str::<Record>(l).unwrap().seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    // and a later open/resync sees a clean file
    let mut reopened = Journal::open(&root, "session").unwrap();
    assert_eq!(reopened.next_seq(), 5);
    reopened.resync().unwrap();
    fs::remove_dir_all(root).ok();
}

/// forbids secrets in the journal. The filter existed and was wired
/// to the diary and to MEMORY.md, but not here — so a command that echoed
/// a token wrote it verbatim into `.sqwai/journal/*.jsonl`, and from there
/// into the diary host block and the compaction anchor, both of which are
/// built from these records.
#[test]
fn secrets_are_screened_out_of_summaries_and_notes() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal
        .append(
            "tool_result",
            json!({
                "tool": "bash",
                "ok": true,
                "summary": "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
            }),
        )
        .unwrap();
    journal
        .append(
            "note",
            json!({"by": "model", "note": "lesson", "text": "the token is ghp_abcdefghijklmnopqrstuvwxyz0123456789"}),
        )
        .unwrap();
    // a path is not a secret and has to survive, or the record stops being
    // able to say what happened
    journal
        .append(
            "tool_result",
            json!({"tool": "write", "ok": true, "summary": "wrote .sqwai/plans/01M1V0GK22W0PFVYBM0501N1FJ.json (+31/-31)"}),
        )
        .unwrap();

    let raw = fs::read_to_string(root.join(".sqwai/journal/session.jsonl")).unwrap();
    assert!(!raw.contains("sk-ant-api03"), "the key reached the journal");
    assert!(!raw.contains("ghp_abcdef"), "the token reached the journal");
    assert!(raw.contains("[redacted]"));
    assert!(
        raw.contains("01M1V0GK22W0PFVYBM0501N1FJ.json"),
        "the plan path must survive screening: {raw}"
    );
    fs::remove_dir_all(root).ok();
}

/// a record taken while nothing is in progress carries `step: null`
/// and is still a session fact. Dropping it emptied the diary host block
/// and the compaction anchor for every session without an active plan.
#[test]
fn unattributed_results_are_recorded_as_session_facts() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    // no set_attribution: no active plan, nothing in progress
    assert_eq!(
        journal
            .append_evidence("tool_result", json!({"tool": "bash", "ok": true}))
            .unwrap(),
        1
    );
    assert_eq!(
        journal
            .append_evidence("file_diff", json!({"path": "src/main.rs", "added": 3}))
            .unwrap(),
        2
    );

    let records = Journal::records_for(&root, "session").unwrap();
    assert_eq!(records.len(), 2, "records must reach the journal");
    assert_eq!(records[0].kind, "tool_result");
    assert_eq!(records[1].kind, "file_diff");
    for record in &records {
        assert!(
            record.step.is_none(),
            "unattributed record needs step: null"
        );
        assert!(record.plan.is_none());
    }

    // ... and are never usable as evidence
    for (index, record) in records.iter().enumerate() {
        let reference = crate::plan::EvidenceRef {
            session: "session".to_string(),
            seq: record.seq,
        };
        assert!(
            Journal::evidence(&root, "any-plan", Some("1"), &reference, None)
                .unwrap()
                .is_none(),
            "record {index} must not qualify as evidence"
        );
    }
    fs::remove_dir_all(root).ok();
}

/// The pre-image to return to is the state *before* the window, so a file
/// written twice must revert to the first record's blob, not the last —
/// while the outside-edit check still compares against the newest hash the
/// host left. Getting this backwards would restore an intermediate state
/// and call it done.
#[test]
fn a_file_written_twice_reverts_to_the_state_before_the_window() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    for (blob, hash) in [
        ("blake3:first", "sha256:one"),
        ("blake3:second", "sha256:two"),
    ] {
        journal
            .append(
                "file_diff",
                serde_json::json!({
                    "path": "src/main.rs",
                    "checkpoint": "cp1",
                    "blob_before": blob,
                    "hash_after": hash,
                }),
            )
            .unwrap();
    }
    let pre = Journal::recorded_pre_images(&root, &["cp1".to_string()]).unwrap();
    assert_eq!(pre.len(), 1, "one entry per path: {pre:?}");
    assert_eq!(pre[0].blob_before.as_deref(), Some("blake3:first"));
    assert_eq!(pre[0].agent_hash.as_deref(), Some("sha256:two"));
}

/// A record from before the blob store existed has no pre-image. It must
/// not be read as "this file was created" — that would delete a file the
/// agent merely edited.
#[test]
fn a_record_without_a_blob_is_not_mistaken_for_a_created_file() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal
        .append(
            "file_diff",
            serde_json::json!({
                "path": "legacy.rs",
                "checkpoint": "cp1",
                "hash_before": "sha256:old",
                "hash_after": "sha256:new",
            }),
        )
        .unwrap();
    let pre = Journal::recorded_pre_images(&root, &["cp1".to_string()]).unwrap();
    assert_eq!(pre[0].blob_before, None);
    assert!(
        pre[0].existed_before,
        "the file existed, so a revert cannot mean deleting it"
    );
    assert_eq!(pre[0].agent_hash.as_deref(), Some("sha256:new"));
}

/// proviso for per-step undo: a file may be reverted alone only if
/// no later step has written it since. Reverting it anyway would put back
/// bytes from before the *later* step, silently undoing that step too —
/// which is worse than refusing, because the plan would still call it done.
#[test]
fn a_step_whose_file_a_later_step_rewrote_is_refused_by_name() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();

    journal.set_attribution(Some("1".into()), Some("plan".into()), "main");
    journal
        .append(
            "file_diff",
            serde_json::json!({
                "path": "src/a.rs",
                "blob_before": "blake3:a-before-step-1",
                "hash_before": "sha256:a0",
                "hash_after": "sha256:a1",
            }),
        )
        .unwrap();
    journal
        .append(
            "file_diff",
            serde_json::json!({
                "path": "src/untouched.rs",
                "blob_before": "blake3:u-before-step-1",
                "hash_before": "sha256:u0",
                "hash_after": "sha256:u1",
            }),
        )
        .unwrap();

    // a later step rewrites one of the two files
    journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
    journal
        .append(
            "file_diff",
            serde_json::json!({
                "path": "src/a.rs",
                "blob_before": "blake3:a-before-step-2",
                "hash_before": "sha256:a1",
                "hash_after": "sha256:a2",
            }),
        )
        .unwrap();

    let revert = Journal::step_pre_images_in(&root, None, "1").unwrap();
    assert_eq!(
        revert.written_since,
        vec!["src/a.rs".to_string()],
        "the contested file has to be named, not silently dropped"
    );
    assert_eq!(
        revert.files.len(),
        1,
        "the other file is still revertible: {:?}",
        revert.files
    );
    assert_eq!(revert.files[0].path, "src/untouched.rs");
    assert_eq!(
        revert.files[0].blob_before.as_deref(),
        Some("blake3:u-before-step-1")
    );

    // and step 2 itself, being the latest writer, reverts freely
    let revert = Journal::step_pre_images_in(&root, None, "2").unwrap();
    assert!(revert.written_since.is_empty());
    assert_eq!(
        revert.files[0].blob_before.as_deref(),
        Some("blake3:a-before-step-2"),
        "step 2 goes back to the state step 1 left, not to the original"
    );
}

/// `seq` is per session file: a later write from a quiet session must
/// still block the revert (no silent clobber), and an earlier write
/// from a busy session must not (no false refusal). The wall clock
/// decides across sessions, the file order within one; a cross-session
/// tie refuses, because the order is genuinely unknown. One sleep: the
/// late pair lands a full second after the early pair.
#[test]
fn written_since_orders_across_sessions_by_time_not_seq() {
    let root = root();
    let mut sess_a = Journal::open(&root, "sess-a").unwrap();
    let mut sess_b = Journal::open(&root, "sess-b").unwrap();
    let diff = |path: &str, before: &str| {
        serde_json::json!({
            "path": path,
            "blob_before": before,
            "hash_before": "sha256:h",
            "hash_after": "sha256:h2",
        })
    };
    sess_a.set_attribution(Some("1".into()), Some("plan-a".into()), "main");
    sess_b.set_attribution(Some("2".into()), Some("plan-a".into()), "main");
    // early pair (T0): busy session A writes X at high seq, quiet
    // session B writes Y at seq 1
    for i in 0..8 {
        sess_a.append("note", serde_json::json!({"n": i})).unwrap();
    }
    sess_a
        .append("file_diff", diff("x.rs", "blake3:x-a"))
        .unwrap();
    sess_b
        .append("file_diff", diff("y.rs", "blake3:y-b"))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    // late pair (T1, low seqs): B rewrites X, A writes Y
    sess_b
        .append("file_diff", diff("x.rs", "blake3:x-b"))
        .unwrap();
    sess_a
        .append("file_diff", diff("y.rs", "blake3:y-a"))
        .unwrap();

    // step 1's X predates step 2's rewrite: reverting alone would
    // clobber it, so X is refused; Y is step 1's own latest word.
    let revert = Journal::step_pre_images_in(&root, Some("plan-a"), "1").unwrap();
    assert_eq!(revert.written_since, vec!["x.rs".to_string()]);
    assert_eq!(
        revert
            .files
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>(),
        vec!["y.rs".to_string()]
    );
    assert_eq!(revert.files[0].blob_before.as_deref(), Some("blake3:y-a"));
    // step 2's X is its own latest word (step 1's early X predates it
    // despite the busy session's high counters); step 1's late Y ties
    // it to the second, so Y is refused as ambiguous.
    let revert = Journal::step_pre_images_in(&root, Some("plan-a"), "2").unwrap();
    assert_eq!(revert.written_since, vec!["y.rs".to_string()]);
    assert_eq!(
        revert
            .files
            .iter()
            .map(|item| item.path.clone())
            .collect::<Vec<_>>(),
        vec!["x.rs".to_string()]
    );
    assert_eq!(revert.files[0].blob_before.as_deref(), Some("blake3:x-b"));
    fs::remove_dir_all(root).ok();
}

/// Records from other steps must not leak into a step's own revert, and a
/// step that wrote nothing has nothing to put back.
#[test]
fn a_step_reverts_only_its_own_writes() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal.set_attribution(Some("7".into()), Some("plan".into()), "main");
    journal
        .append(
            "file_diff",
            serde_json::json!({"path": "mine.rs", "blob_before": "blake3:mine"}),
        )
        .unwrap();
    journal.set_attribution(None, Some("plan".into()), "main");
    journal
        .append(
            "file_diff",
            serde_json::json!({"path": "unattributed.rs", "blob_before": "blake3:other"}),
        )
        .unwrap();

    let revert = Journal::step_pre_images_in(&root, None, "7").unwrap();
    assert_eq!(revert.files.len(), 1);
    assert_eq!(revert.files[0].path, "mine.rs");

    assert!(
        Journal::step_pre_images_in(&root, None, "9")
            .unwrap()
            .files
            .is_empty(),
        "a step with no records reverts nothing"
    );
}

/// an assumption is open until a note closes it by seq.
/// Without the closure the kind exists but never settles.
#[test]
fn an_assumption_is_open_until_a_note_resolves_it_by_seq() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
    let first = journal
        .append(
            "note",
            serde_json::json!({"by": "model", "note": "assumption", "text": "the API returns UTF-8"}),
        )
        .unwrap();
    let second = journal
        .append(
            "note",
            serde_json::json!({"by": "model", "note": "assumption", "text": "the cache is warm"}),
        )
        .unwrap();
    // an unrelated note must not close anything
    journal
        .append(
            "note",
            serde_json::json!({"by": "model", "note": "decision", "text": "use blake3"}),
        )
        .unwrap();

    let open = Journal::open_assumptions_in(&root, "session", None).unwrap();
    assert_eq!(
        open.iter().map(|item| item.seq).collect::<Vec<_>>(),
        vec![first, second]
    );

    // closing the first leaves exactly the second
    journal
        .append(
            "note",
            serde_json::json!({
                "by": "model",
                "note": "assumption",
                "text": "confirmed by the test",
                "resolves": first,
            }),
        )
        .unwrap();
    let open = Journal::open_assumptions_in(&root, "session", None).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].seq, second);
    assert!(
        open[0]
            .label(80)
            .starts_with(&format!("j#{second}: the cache"))
    );
}

/// `finish` warns about the step it is finishing, not about the whole
/// session: an assumption belonging to another step is not this step's
/// business.
#[test]
fn open_assumptions_can_be_narrowed_to_one_step() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal.set_attribution(Some("1".into()), Some("plan".into()), "main");
    journal
        .append(
            "note",
            serde_json::json!({"by": "model", "note": "assumption", "text": "step one's"}),
        )
        .unwrap();
    journal.set_attribution(Some("2".into()), Some("plan".into()), "main");
    journal
        .append(
            "note",
            serde_json::json!({"by": "model", "note": "assumption", "text": "step two's"}),
        )
        .unwrap();

    let one = Journal::open_assumptions_in(&root, "session", Some("1")).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].text, "step one's");
    assert_eq!(
        Journal::open_assumptions_in(&root, "session", Some("3"))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        Journal::open_assumptions_in(&root, "session", None)
            .unwrap()
            .len(),
        2
    );
}

/// Step numbers restart per plan ("1" exists in every plan), so a
/// project-wide scan merges same-numbered steps of different plans.
/// The scoped revert sees only its own plan's writes — including the
/// "later step rewrote it" guard, which must not fire on another
/// plan's same-named file.
#[test]
fn step_pre_images_are_scoped_to_one_plan() {
    let root = root();
    let mut a = Journal::open(&root, "sess-a").unwrap();
    a.set_attribution(Some("1".into()), Some("plan-a".into()), "main");
    a.append(
        "file_diff",
        json!({
            "path": "a.rs",
            "blob_before": "blake3:a",
            "hash_before": "h0",
            "hash_after": "h1",
        }),
    )
    .unwrap();
    let mut b = Journal::open(&root, "sess-b").unwrap();
    b.set_attribution(Some("1".into()), Some("plan-b".into()), "main");
    b.append(
        "file_diff",
        json!({
            "path": "b.rs",
            "blob_before": "blake3:b",
            "hash_before": "h0",
            "hash_after": "h1",
        }),
    )
    .unwrap();
    // another plan's step 2 rewrites the same path: unscoped, that
    // would refuse plan-a's revert of a.rs
    b.set_attribution(Some("2".into()), Some("plan-b".into()), "main");
    b.append(
        "file_diff",
        json!({
            "path": "a.rs",
            "blob_before": "blake3:a2",
            "hash_before": "h1",
            "hash_after": "h2",
        }),
    )
    .unwrap();

    let revert = Journal::step_pre_images_in(&root, Some("plan-a"), "1").unwrap();
    assert_eq!(revert.files.len(), 1, "{revert:?}");
    assert_eq!(revert.files[0].path, "a.rs");
    assert!(
        revert.written_since.is_empty(),
        "foreign plan writes must not block: {revert:?}"
    );
    let other = Journal::step_pre_images_in(&root, Some("plan-b"), "1").unwrap();
    assert_eq!(other.files.len(), 1);
    assert_eq!(other.files[0].path, "b.rs");
}

/// Sequence numbers restart per journal file, so a `resolves` seq is
/// only meaningful inside its own session — and neither are open
/// assumption lists.
#[test]
fn assumptions_stay_in_their_session() {
    let root = root();
    let mut a = Journal::open(&root, "sess-a").unwrap();
    a.set_attribution(Some("1".into()), Some("plan-a".into()), "main");
    a.append(
        "note",
        serde_json::json!({"by": "model", "note": "assumption", "text": "a holds"}),
    )
    .unwrap();
    let mut b = Journal::open(&root, "sess-b").unwrap();
    b.set_attribution(Some("1".into()), Some("plan-b".into()), "main");
    b.append(
        "note",
        serde_json::json!({"by": "model", "note": "assumption", "text": "b holds"}),
    )
    .unwrap();

    let open_a = Journal::open_assumptions_in(&root, "sess-a", None).unwrap();
    assert_eq!(open_a.len(), 1);
    assert_eq!(open_a[0].text, "a holds");
    let open_b = Journal::open_assumptions_in(&root, "sess-b", Some("1")).unwrap();
    assert_eq!(open_b.len(), 1);
    assert_eq!(open_b[0].text, "b holds");
}

/// What scopes `/undo`: only the paths this checkpoint's own `file_diff`
/// records name, each with the hash the agent left the file at.
#[test]
fn recorded_writes_scopes_undo_to_the_hosts_own_writes() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal
        .append(
            "file_diff",
            json!({"path": "src/a.rs", "hash_after": "aaa", "checkpoint": "sha-1"}),
        )
        .unwrap();
    journal
        .append(
            "file_diff",
            json!({"path": "src/b.rs", "hash_after": "bbb", "checkpoint": "sha-2"}),
        )
        .unwrap();
    // same file written twice inside the undone window: the newest hash is
    // the one it was left at
    journal
        .append(
            "file_diff",
            json!({"path": "src/a.rs", "hash_after": "aaa2", "checkpoint": "sha-2"}),
        )
        .unwrap();
    // a record belonging to a checkpoint that is not being undone
    journal
        .append(
            "file_diff",
            json!({"path": "src/untouched.rs", "hash_after": "zzz", "checkpoint": "sha-9"}),
        )
        .unwrap();

    let writes =
        Journal::recorded_writes(&root, &["sha-1".to_string(), "sha-2".to_string()]).unwrap();
    assert_eq!(
        writes,
        vec![
            ("src/a.rs".to_string(), Some("aaa2".to_string())),
            ("src/b.rs".to_string(), Some("bbb".to_string())),
        ]
    );

    // a checkpoint with no file records — a bash mutation — cannot narrow
    // the scope, and says so by returning nothing
    assert!(
        Journal::recorded_writes(&root, &["sha-bash".to_string()])
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn records_undo_checkpoint_files_and_reopened_steps() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    journal.set_attribution(None, Some("plan-1".into()), "host");
    let seq = journal
        .append_undo(
            "checkpoint-1",
            &["src/lib.rs".into(), "src/main.rs".into()],
            &["1".into()],
        )
        .unwrap();

    let record = Journal::records_for(&root, "session")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(seq, record.seq);
    assert_eq!(record.kind, "undo");
    assert_eq!(record.agent, "host");
    assert_eq!(record.plan.as_deref(), Some("plan-1"));
    assert_eq!(record.fields["to_checkpoint"], "checkpoint-1");
    assert_eq!(record.fields["files"], json!(["src/lib.rs", "src/main.rs"]));
    assert_eq!(record.fields["reopened_steps"], json!(["1"]));
    fs::remove_dir_all(root).ok();
}

#[test]
fn repairs_partial_tail_and_records_that_it_did() {
    let root = root();
    let path = root.join(".sqwai").join("journal").join("session.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{\"seq\":1,\"ts\":\"1\",\"step\":null,\"plan\":null,\"agent\":\"main\",\"kind\":\"note\",\"text\":\"ok\"}\n{\"seq\":2").unwrap();
    let mut journal = Journal::open(&root, "session").unwrap();

    // the partial line goes, and the host says so. Truncating an
    // append-only log in silence is the one thing an audit record cannot do.
    let records = Journal::records_for(&root, "session").unwrap();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1].kind, "journal_repair");
    assert!(
        records[1]
            .fields
            .get("truncated_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|bytes| bytes > 0)
    );

    // and the sequence continues past it
    assert_eq!(journal.next_seq(), 3);
    assert_eq!(journal.append("note", json!({"text": "next"})).unwrap(), 3);
    assert_eq!(
        fs::read_to_string(journal.path()).unwrap().lines().count(),
        3
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn nudges_after_unaccounted_step_actions() {
    let root = root();
    let mut plan = crate::plan::create(
        "keep working".to_string(),
        Vec::new(),
        vec!["note".to_string()],
        vec![crate::plan::NewStep {
            title: "step".into(),
        }],
        &crate::plan::Limits::default(),
    )
    .unwrap();
    crate::plan::apply(
        &mut plan,
        crate::plan::Op::Join {
            session: "nudge".into(),
        },
        &crate::plan::Limits::default(),
    )
    .unwrap();
    crate::plan::store(&root, &plan).unwrap();
    let mut journal = Journal::open(&root, "nudge").unwrap();
    journal.set_attribution(Some("1".into()), Some(plan.id.clone()), "main");
    for _ in 0..3 {
        journal
            .append("tool_result", json!({"tool": "read", "ok": true}))
            .unwrap();
    }
    // Session "nudge" joins at create time in the live loop (created.sessions
    // carries the creating session); the test mirrors that membership so
    // open_active_for_session resolves this session's own plan.
    assert!(
        Journal::nudge(&root, Some("nudge"), 2)
            .unwrap()
            .unwrap()
            .contains("3 actions")
    );
    journal.append("plan", json!({"op": "add"})).unwrap();
    assert!(Journal::nudge(&root, Some("nudge"), 2).unwrap().is_none());
    fs::remove_dir_all(root).ok();
}

#[test]
fn rejects_non_object_fields() {
    let root = root();
    let mut journal = Journal::open(&root, "session").unwrap();
    assert!(journal.append("bad", json!("nope")).is_err());
    fs::remove_dir_all(root).ok();
}

#[test]
fn writer_stamps_inherited_epoch_and_defaults_to_none() {
    let root = root();
    let mut journal = Journal::open(&root, "epoch-stamp").unwrap();
    let plain = journal
        .append("note", json!({"text": "main work"}))
        .unwrap();
    journal.set_epoch(Some(3));
    let stamped = journal
        .append("note", json!({"text": "child work"}))
        .unwrap();

    let records = Journal::records_for(&root, "epoch-stamp").unwrap();
    let by_seq = |seq| records.iter().find(|r| r.seq == seq).unwrap().epoch;
    assert_eq!(by_seq(plain), None);
    assert_eq!(by_seq(stamped), Some(3));
    fs::remove_dir_all(root).ok();
}

/// Concurrent handles on one journal file must never duplicate a seq:
/// without the per-file append lock two handles read the same tail and
/// write the same number twice, poisoning replay cursors downstream.
/// One malformed line mid-file (a crash, a disk error, a hand edit)
/// used to fail the whole read — every caller of `records()` (undo,
/// blob purge, evidence validation) died project-wide (audit H18).
/// Degrade, don't refuse: unreadable lines are skipped, the readable
/// history survives.
#[test]
fn records_skip_a_corrupt_mid_file_line() {
    let dir = tempfile::tempdir().unwrap();
    let jdir = dir.path().join(".sqwai/journal");
    std::fs::create_dir_all(&jdir).unwrap();
    std::fs::write(
        jdir.join("s.jsonl"),
        concat!(
            "{\"seq\":1,\"ts\":\"t\",\"agent\":\"main\",\"kind\":\"note\",\"text\":\"a\"}\n",
            "{\"seq\":2,\"ts\":\"t\",\"agent\":\"main\",\"kind\":\"no\n",
            "{\"seq\":3,\"ts\":\"t\",\"agent\":\"main\",\"kind\":\"note\",\"text\":\"c\"}\n",
        ),
    )
    .unwrap();
    let records =
        Journal::records(dir.path()).expect("a corrupt line must not kill the whole read");
    let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 3]);
    assert_eq!(records[0].session, "s", "session stamp survives");
}

#[test]
fn concurrent_handles_never_duplicate_seq() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".sqwai/journal")).unwrap();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let mut journal = Journal::open(&root, "race").expect("open");
            let mut seqs = Vec::new();
            for i in 0..25 {
                seqs.push(
                    journal
                        .append("note", serde_json::json!({"text": i}))
                        .expect("append"),
                );
            }
            seqs
        }));
    }
    let mut all = Vec::new();
    for handle in handles {
        all.extend(handle.join().expect("thread"));
    }
    assert_eq!(all.len(), 200);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 200, "duplicate seq numbers were written");
}
