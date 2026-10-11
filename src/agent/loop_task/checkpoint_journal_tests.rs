use super::checkpoint_fields;

/// One commit, one record, and the record names the edge it is. Step
/// boundaries used to be journaled twice — once as `shadow/step_start`,
/// once as `legacy/post_mutation` — where "legacy" described the very same
/// shadow snapshot layer, so the ledger claimed a mutation that never
/// happened.
#[test]
fn a_step_boundary_is_journaled_as_a_boundary_not_a_mutation() {
    let start = checkpoint_fields("sha1", "step_2_start");
    assert_eq!(start["layer"], "shadow");
    assert_eq!(start["reason"], "step_start");
    assert_eq!(start["step"], "2");
    assert!(
        start.get("label").is_none(),
        "a boundary carries no phantom mutation label: {start}"
    );
    let finish = checkpoint_fields("sha2", "step_2_finish");
    assert_eq!(finish["reason"], "step_finish");
    assert_eq!(finish["step"], "2");
}

#[test]
fn a_mutation_snapshot_names_what_changed() {
    for (label, want) in [
        ("edit src/main.rs", "edit src/main.rs"),
        ("bash cargo build", "bash cargo build"),
    ] {
        let rec = checkpoint_fields("sha3", label);
        assert_eq!(rec["layer"], "shadow");
        assert_eq!(rec["reason"], "post_mutation");
        assert_eq!(rec["label"], want);
    }
}
