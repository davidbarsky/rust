use super::{ShouldFail, TestFailure, TestOutcome};

#[test]
fn expected_failure_message_matches_exactly() {
    let failure = TestFailure("expected failure".to_owned());

    assert_eq!(
        ShouldFail::Message("expected failure".to_owned()).outcome(Some(&failure)),
        TestOutcome::Succeeded,
    );
    assert_eq!(
        ShouldFail::Message("different failure".to_owned()).outcome(Some(&failure)),
        TestOutcome::Failed {
            message: Some("`//@ should-fail` test reported a different failure than expected")
        },
    );
}

#[test]
fn expected_failure_message_rejects_an_untyped_panic() {
    let panic = "expected failure".to_owned();

    assert_eq!(
        ShouldFail::Message("expected failure".to_owned()).outcome(Some(&panic)),
        TestOutcome::Failed {
            message: Some(
                "`//@ should-fail` test panicked instead of reporting the expected failure"
            )
        },
    );
}
