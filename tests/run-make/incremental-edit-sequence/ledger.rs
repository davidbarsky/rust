#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Witness {
    MutationInversePairing,
    EdgeLocalScheduling,
    ObservationRecorded,
    IncrementalCompilation,
    ModelProbeAgreement,
    CleanIncrementalProbeEquivalence,
    ArtifactRestoration,
    HistoryIndependentCleanArtifacts,
    CleanIncrementalMirEquivalence,
    ModelOutcomeAgreement,
    CleanIncrementalAcceptanceEquivalence,
    CleanIncrementalMetadataEquality,
    CleanIncrementalCodegenEquality,
    CleanIncrementalDiagnosticsEquality,
    ExpectedMetadataState,
}

/// Runs `check` and returns its value, or fails the history naming the witness that rejected it.
/// Returning the success value lets a witness both judge and yield a result, so a caller never has
/// to re-match a value the witness already proved.
pub(crate) fn witness<T>(witness: Witness, check: impl FnOnce() -> Result<T, String>) -> T {
    match check() {
        Ok(value) => value,
        Err(message) => panic!("{witness:?}: {message}"),
    }
}
