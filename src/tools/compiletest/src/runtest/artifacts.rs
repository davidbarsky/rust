use std::collections::hash_map::Entry;

use build_helper::dep_info::MakeDepInfo;
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::directives::ByteExpectation;
use crate::runtest::AuxType;

#[cfg(test)]
mod tests;

type FxIndexMap<K, V> = indexmap::IndexMap<K, V, rustc_hash::FxBuildHasher>;

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
