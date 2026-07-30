use rustc_data_structures::stable_hash::{SpanHashMode, StableHash, StableHashCtxt, StableHasher};
use rustc_hir::def::Res;
use rustc_macros::{StableHash, TyDecodable, TyEncodable};
use rustc_span::Ident;
use rustc_span::def_id::{DefId, ModId};
use smallvec::SmallVec;

use crate::ty;

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

/// A simplified version of `ImportKind` from resolve.
/// `DefId`s here correspond to `use` and `extern crate` items themselves, not their targets.
#[derive(Clone, Copy, Debug, TyEncodable, TyDecodable, StableHash)]
pub enum Reexport {
    Single(DefId),
    Glob(DefId),
    ExternCrate(DefId),
    MacroUse,
    MacroExport,
}

impl Reexport {
    pub fn id(self) -> Option<DefId> {
        match self {
            Reexport::Single(id) | Reexport::Glob(id) | Reexport::ExternCrate(id) => Some(id),
            Reexport::MacroUse | Reexport::MacroExport => None,
        }
    }
}

/// This structure is supposed to keep enough data to re-create `Decl`s for other crates
/// during name resolution. Right now the bindings are not recreated entirely precisely so we may
/// need to add more data in the future to correctly support macros 2.0, for example.
/// Module child can be either a proper item or a reexport (including private imports).
/// In case of reexport all the fields describe the reexport item itself, not what it refers to.
#[derive(Debug, TyEncodable, TyDecodable, StableHash)]
pub struct ModChild {
    /// Name of the item.
    pub ident: Ident,
    /// Resolution result corresponding to the item.
    /// Local variables cannot be exported, so this `Res` doesn't need the ID parameter.
    pub res: Res<!>,
    /// Visibility of the item.
    pub vis: ty::Visibility<ModId>,
    /// Reexport chain linking this module child to its original reexported item.
    /// Empty if the module child is a proper item.
    pub reexport_chain: SmallVec<[Reexport; 2]>,
}

/// Same as `ModChild`, however, it includes ambiguity error.
#[derive(Debug, TyEncodable, TyDecodable, StableHash)]
pub struct AmbigModChild {
    pub main: ModChild,
    pub second: ModChild,
}
