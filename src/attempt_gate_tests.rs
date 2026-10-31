use crate::attempt_gate::AttemptGate;

#[test]
fn a_fresh_gate_is_active_until_commit_or_discard() {
    let gate = AttemptGate::new();
    assert!(gate.is_active(), "a new attempt can continue");
    assert!(gate.try_commit(), "nothing has claimed the attempt yet");
    assert!(!gate.is_active(), "a committing attempt no longer accepts work");

    let discarded = AttemptGate::new();
    assert!(discarded.is_active());
    assert!(discarded.discard());
    assert!(!discarded.is_active(), "a discarded attempt no longer accepts work");
}

#[test]
fn a_gate_commits_only_once() {
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.try_commit(), "a second commit must not also win");
}

#[test]
fn a_discard_before_a_commit_blocks_the_commit() {
    // The window the design closes: a first removal wins outright, worker delivers nothing.
    let gate = AttemptGate::new();
    assert!(gate.discard(), "the first discard wins");
    assert!(!gate.discard(), "a second discard must not also win");
    assert!(!gate.try_commit(), "a discarded attempt must not deliver");
}

#[test]
fn a_commit_before_a_discard_blocks_the_discard() {
    // Mirror: commit wins, so the finalizer needs `was_delivered` to know if a file was placed.
    let gate = AttemptGate::new();
    assert!(gate.try_commit());
    assert!(!gate.discard(), "the commit already had it");
}

#[test]
fn delivery_is_recorded_only_after_marking() {
    let gate = AttemptGate::new();
    assert!(!gate.was_delivered());
    gate.mark_delivered();
    assert!(gate.was_delivered());
}

#[test]
fn a_shared_gate_is_the_same_gate() {
    // The manager and the worker must arbitrate on one object, not copies.
    let gate = AttemptGate::new();
    let worker = std::sync::Arc::clone(&gate);
    assert!(worker.discard());
    assert!(
        !gate.try_commit(),
        "the other handle did not observe the discard"
    );
}
