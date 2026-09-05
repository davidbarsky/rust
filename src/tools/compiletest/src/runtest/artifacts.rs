use std::collections::hash_map::Entry;
use std::process::Command;
use std::str::FromStr;
use std::{fmt, fs};

use build_helper::dep_info::MakeDepInfo;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::directives::ByteExpectation;
use crate::runtest::AuxType;

#[cfg(test)]
mod tests;

type FxIndexMap<K, V> = indexmap::IndexMap<K, V, rustc_hash::FxBuildHasher>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandFingerprint(String);

impl CommandFingerprint {
    pub(crate) fn from_command(
        command: &Command,
        revision_source: Option<(&Utf8Path, &Utf8Path)>,
    ) -> Self {
        let mut fingerprint = String::new();
        for arg in command.get_args() {
            let arg = match revision_source {
                Some((physical, logical)) if arg == physical.as_os_str() => logical.as_os_str(),
                Some(_) | None => arg,
            };
            fingerprint.push_str(&arg.to_string_lossy());
            fingerprint.push('\0');
        }
        let mut envs: Vec<String> =
            command.get_envs().map(|(key, value)| format!("{key:?}={value:?}")).collect();
        envs.sort();
        for env in envs {
            fingerprint.push_str(&env);
            fingerprint.push('\0');
        }
        Self(fingerprint)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BuildRecord {
    pub(crate) revision: String,
    pub(crate) command: CommandFingerprint,
}

impl FromStr for BuildRecord {
    type Err = ();

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (revision, command) = input.split_once('\n').ok_or(())?;
        Ok(Self { revision: revision.to_owned(), command: CommandFingerprint(command.to_owned()) })
    }
}

impl fmt::Display for BuildRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{}", self.revision, self.command.0)
    }
}

pub(crate) struct RevisionInputs<'a> {
    pub(crate) source_root: &'a Utf8Path,
    pub(crate) revision_source_candidates: &'a [Utf8PathBuf],
    pub(crate) crate_source: &'a Utf8Path,
    pub(crate) input_path: &'a Utf8Path,
    pub(crate) snapshot_root: &'a Utf8Path,
}

pub(crate) struct ReuseRequest<'a> {
    pub(crate) current_revision: Option<&'a str>,
    pub(crate) previous_revision: Option<&'a str>,
    pub(crate) command: &'a CommandFingerprint,
    pub(crate) inputs: Option<RevisionInputs<'a>>,
}

#[derive(Debug)]
pub(crate) struct ReusePlan {
    record: BuildRecord,
}

impl ReusePlan {
    pub(crate) fn into_record(self) -> BuildRecord {
        self.record
    }
}

#[derive(Debug)]
pub(crate) enum BuildPlan {
    Invoke,
    Reuse(ReusePlan),
}

impl BuildPlan {
    pub(crate) fn parse(
        record: Option<BuildRecord>,
        dep_info: MakeDepInfo,
        provider_deltas: &ProviderDeltas,
        request: ReuseRequest<'_>,
    ) -> Result<Self, String> {
        if should_rerun(provider_deltas, &dep_info)? {
            return Ok(Self::Invoke);
        }
        let ReuseRequest { current_revision, previous_revision, command, inputs } = request;
        let (Some(record), Some(previous_revision), Some(current_revision)) =
            (record, previous_revision, current_revision)
        else {
            return Ok(Self::Invoke);
        };
        if record.revision != previous_revision || record.command != *command {
            return Ok(Self::Invoke);
        }
        if let Some(RevisionInputs {
            source_root,
            revision_source_candidates,
            crate_source,
            input_path,
            snapshot_root,
        }) = inputs
        {
            let layout = logical_source_layout(
                &dep_info,
                source_root,
                revision_source_candidates,
                crate_source,
            )?;
            let crate_name =
                crate_source.file_name().expect("auxiliary source file has a file name");
            for (logical, physical) in &layout {
                let physical = if logical == crate_name { input_path } else { physical };
                let current = fs::read(physical)
                    .map_err(|err| format!("failed to read own input `{physical}`: {err}"))?;
                let snapshot = snapshot_root.join(logical);
                let snapshot = fs::read(&snapshot)
                    .map_err(|err| format!("failed to read input snapshot `{snapshot}`: {err}"))?;
                if current != snapshot {
                    return Err(format!(
                        "`//@ rustc-not-invoked` on `{crate_source}`, but its own input `{logical}` \
                         changed since the previous build"
                    ));
                }
            }
        }
        Ok(Self::Reuse(ReusePlan {
            record: BuildRecord { revision: current_revision.to_owned(), command: record.command },
        }))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ArtifactProvider {
    pub(crate) source: Utf8PathBuf,
    pub(crate) out_dir: Utf8PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ArtifactPath {
    Rmeta(Utf8PathBuf),
    Link(Utf8PathBuf),
}

impl AsRef<Utf8Path> for ArtifactPath {
    fn as_ref(&self) -> &Utf8Path {
        match self {
            Self::Rmeta(path) => path,
            Self::Link(path) => path,
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ArtifactSnapshot(pub(crate) FxHashMap<ArtifactPath, Vec<u8>>);

impl ArtifactSnapshot {
    pub(crate) fn changed_paths_since(&self, previous: &Self) -> FxHashSet<ArtifactPath> {
        let mut changed = FxHashSet::default();
        for (path, bytes) in &self.0 {
            if previous.0.get(path) != Some(bytes) {
                changed.insert(path.clone());
            }
        }
        for path in previous.0.keys() {
            if !self.0.contains_key(path) {
                changed.insert(path.clone());
            }
        }
        changed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderDelta {
    Measured(FxHashSet<ArtifactPath>),
    Unmeasured,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderDeltas(pub(crate) FxHashMap<ArtifactProvider, ProviderDelta>);

impl Extend<(ArtifactProvider, ProviderDelta)> for ProviderDeltas {
    fn extend<T: IntoIterator<Item = (ArtifactProvider, ProviderDelta)>>(&mut self, deltas: T) {
        for (provider, delta) in deltas {
            match self.0.entry(provider) {
                Entry::Vacant(entry) => {
                    entry.insert(delta);
                }
                Entry::Occupied(mut entry) => match (entry.get_mut(), delta) {
                    (ProviderDelta::Measured(known), ProviderDelta::Measured(more)) => {
                        known.extend(more);
                    }
                    (ProviderDelta::Measured(_), ProviderDelta::Unmeasured)
                    | (ProviderDelta::Unmeasured, ProviderDelta::Measured(_))
                    | (ProviderDelta::Unmeasured, ProviderDelta::Unmeasured) => {
                        entry.insert(ProviderDelta::Unmeasured);
                    }
                },
            }
        }
    }
}

pub(crate) fn logical_source_layout(
    inputs: &MakeDepInfo,
    source_root: &Utf8Path,
    revision_source_candidates: &[Utf8PathBuf],
    crate_source: &Utf8Path,
) -> Result<FxIndexMap<Utf8PathBuf, Utf8PathBuf>, String> {
    let crate_name = crate_source.file_name().expect("auxiliary source file has a file name");
    let mut layout = FxIndexMap::default();
    for input in &inputs.0 {
        let Ok(relative) = input.strip_prefix(source_root) else {
            continue;
        };
        if !relative.components().all(|component| match component {
            Utf8Component::Normal(_) => true,
            Utf8Component::Prefix(_)
            | Utf8Component::RootDir
            | Utf8Component::CurDir
            | Utf8Component::ParentDir => false,
        }) {
            return Err(format!("input `{input}` does not stay inside `{source_root}`"));
        }
        let logical = if revision_source_candidates.iter().any(|c| c.as_path() == relative) {
            Utf8PathBuf::from(crate_name)
        } else {
            relative.to_path_buf()
        };
        if let Some(previous) = layout.insert(logical.clone(), input.to_path_buf()) {
            return Err(format!("inputs `{previous}` and `{input}` both stage as `{logical}`"));
        }
    }
    if !layout.contains_key(Utf8Path::new(crate_name)) {
        return Err(format!(
            "dependency information for `{crate_source}` does not list its root input under \
             `{source_root}`"
        ));
    }
    layout.sort_unstable_keys();
    Ok(layout)
}

pub(crate) fn check_byte_expectations(
    expectations: &[(ArtifactPath, ByteExpectation)],
    previous: &ArtifactSnapshot,
    current: &ArtifactSnapshot,
) -> Result<(), String> {
    for (artifact, expectation) in expectations {
        let path = AsRef::<Utf8Path>::as_ref(artifact);
        match artifact {
            ArtifactPath::Rmeta(_) | ArtifactPath::Link(_) => {
                if !previous.0.contains_key(artifact) || !current.0.contains_key(artifact) {
                    return Err(format!(
                        "artifact `{path}` must exist in both the previous and current snapshots"
                    ));
                }
            }
        }
        match (expectation, previous.0.get(artifact) == current.0.get(artifact)) {
            (ByteExpectation::Same, true) | (ByteExpectation::Different, false) => {}
            (ByteExpectation::Same, false) => {
                return Err(format!("expected artifact bytes to be unchanged: `{path}`"));
            }
            (ByteExpectation::Different, true) => {
                return Err(format!("expected artifact bytes to change: `{path}`"));
            }
        }
    }
    Ok(())
}

pub(crate) fn should_rerun(
    provider_deltas: &ProviderDeltas,
    dep_info: &MakeDepInfo,
) -> Result<bool, String> {
    let identify = |path: &Utf8Path| path.canonicalize_utf8().unwrap_or_else(|_| path.to_owned());
    let observed: FxHashSet<Utf8PathBuf> = dep_info.0.iter().map(|path| identify(path)).collect();
    let mut rerun = false;
    for (provider, delta) in &provider_deltas.0 {
        let ProviderDelta::Measured(changed_paths) = delta else {
            return Err(format!(
                "provider `{}` was built without measured artifacts, so a skip cannot be justified",
                provider.source
            ));
        };
        if changed_paths
            .iter()
            .any(|path| observed.contains(&identify(AsRef::<Utf8Path>::as_ref(path))))
        {
            rerun = true;
        }
    }
    Ok(rerun)
}

#[derive(Clone, Debug)]
pub(crate) struct AuxiliaryBuild {
    pub(crate) aux_type: AuxType,
    pub(crate) provider: ArtifactProvider,
    pub(crate) own_delta: ProviderDelta,
    pub(crate) transitive_deltas: ProviderDeltas,
    pub(crate) rmeta_path: Option<Utf8PathBuf>,
}

pub(crate) struct ProviderArtifacts {
    pub(crate) rmeta: Utf8PathBuf,
    pub(crate) snapshot_rmeta: Utf8PathBuf,
    pub(crate) link: Utf8PathBuf,
    pub(crate) snapshot_link: Utf8PathBuf,
}
