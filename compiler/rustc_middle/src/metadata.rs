use rustc_data_structures::stable_hash::{SpanHashMode, StableHash, StableHashCtxt, StableHasher};

/// Keeps source coordinates out of metadata's semantic incremental dependencies.
///
/// The wrapped value still hashes span hygiene. Position traversal reads ordinary query results,
/// so coordinate-only edits regenerate the spans cache without forcing semantic metadata
/// projection.
#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub struct MetadataSemantic<T>(pub T);

impl<T: StableHash> StableHash for MetadataSemantic<T> {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        hcx.with_span_hash_mode(SpanHashMode::Hygiene, |hcx| {
            self.0.stable_hash(hcx, hasher);
        });
    }
}

impl<T: crate::query::erase::Erasable> crate::query::erase::Erasable for MetadataSemantic<T> {
    type Storage = T::Storage;
}
