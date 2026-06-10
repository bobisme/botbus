//! Integration tests for `rite history -f` (follow mode).

mod common;
use common::TestProject;

/// Regression test: follow mode must not skip messages between the initial
/// bounded read and EOF.
///
/// `history --format json -f --after-offset <X> -n <N>` first emits up to N
/// messages from offset X, then switches to follow mode. The follow cursor must
/// continue from that bounded read's `next_offset` and drain any remaining
/// backlog — not restart from the file's current EOF, which would silently drop
/// the messages in between.
///
/// Here three messages already exist; the bounded read (`-n 1`) emits the first,
/// and follow must still deliver the other two. `--timeout` bounds the run so a
/// regression (which would wait forever for new events) fails instead of hanging.
#[test]
fn follow_from_offset_does_not_skip_backlog() {
    let mut project = TestProject::new();
    let alice = project.agent("Alice");
    let bob = project.agent("Bob");

    alice.send("burst", "msg-one").assert_success();
    alice.send("burst", "msg-two").assert_success();
    alice.send("burst", "msg-three").assert_success();

    // Bounded read emits msg-one; follow must drain msg-two and msg-three.
    let output = bob.run(&[
        "history",
        "burst",
        "--format",
        "json",
        "-f",
        "--after-offset",
        "0",
        "-n",
        "1",
        "--follow-count",
        "2",
        "--timeout",
        "10",
    ]);

    output.assert_success();
    let stdout = output.stdout_str();
    assert!(
        stdout.contains("msg-one") && stdout.contains("msg-two") && stdout.contains("msg-three"),
        "follow mode skipped backlog messages; got:\n{}",
        stdout
    );
}
