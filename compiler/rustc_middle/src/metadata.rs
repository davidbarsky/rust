use std::collections::VecDeque;

use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::{FxHashMap, FxIndexMap, FxIndexSet, IndexEntry};
use rustc_data_structures::stable_hash::{SpanHashMode, StableHash, StableHashCtxt, StableHasher};
use rustc_data_structures::svh::Svh;
use rustc_hir::def::Res;
use rustc_hir::def_id::{CRATE_DEF_ID, DefIndex, DefPathHash, LocalDefId};
use rustc_macros::{
    Decodable_NoContext, Encodable_NoContext, StableHash, TyDecodable, TyEncodable,
};
use rustc_span::def_id::{DefId, ModId};
use rustc_span::hygiene::HygieneEncodeLayout;
use rustc_span::{Ident, Span};
use smallvec::SmallVec;

use crate::ty::{self, TyCtxt};

/// Identifies only the cross-crate semantic contract exposed through metadata.
#[derive(
    Clone,
    Copy,
    Debug,
    Decodable_NoContext,
    Encodable_NoContext,
    Eq,
    Hash,
    PartialEq,
    StableHash
)]
pub struct MetadataContractHash(pub Svh);

impl crate::query::erase::Erasable for MetadataContractHash {
    type Storage = [u8; size_of::<Self>()];
}

/// Describes why an artifact-local definition remains in an RDR metadata artifact.
///
/// A semantic reference makes the definition part of the cross-crate contract. A decode-layout
/// reference retains only the definition needed to interpret that contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DefinitionProjection {
    DecodeLayout,
    Semantic,
}

#[derive(Clone, Copy, Debug)]
struct MetadataDefinition {
    def_id: LocalDefId,
    projection: DefinitionProjection,
}

/// Describes the strongest reference observed for a definition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionState {
    Unselected,
    DecodeLayout,
    Semantic,
}

/// Restricts metadata traversal to definitions selected by the trace.
#[derive(Clone, Copy)]
pub struct SelectedDefinitions<'a> {
    definitions: &'a FxIndexMap<DefPathHash, MetadataDefinition>,
    indices: &'a FxHashMap<DefIndex, DefPathHash>,
}

impl SelectedDefinitions<'_> {
    /// Keeps external definitions available while applying the canonical local selection.
    pub fn includes(&self, def_id: DefId) -> bool {
        match def_id.as_local() {
            Some(def_id) => self.indices.contains_key(&def_id.local_def_index),
            None => true,
        }
    }

    /// Distinguishes absence from either strength of selected owner.
    pub fn state(&self, owner: DefPathHash) -> DefinitionState {
        match self.definitions.get(&owner).map(|definition| definition.projection) {
            Some(DefinitionProjection::DecodeLayout) => DefinitionState::DecodeLayout,
            Some(DefinitionProjection::Semantic) => DefinitionState::Semantic,
            None => DefinitionState::Unselected,
        }
    }
}

/// Identifies the canonical record producer requested by definition tracing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TraceScope {
    Artifact,
    Definition { def_id: LocalDefId, projection: DefinitionProjection },
    Hygiene,
}

/// Collects local definition references without exposing scheduler state.
#[derive(Default)]
pub struct ReferencedDefinitions {
    references: FxIndexMap<LocalDefId, DefinitionProjection>,
    selection_queries: FxIndexSet<LocalDefId>,
}

impl ReferencedDefinitions {
    /// Joins repeated observations so traversal order cannot weaken an existing reference.
    pub fn observe(&mut self, def_id: LocalDefId, projection: DefinitionProjection) {
        match self.references.entry(def_id) {
            IndexEntry::Occupied(mut entry) => {
                *entry.get_mut() = (*entry.get()).max(projection);
            }
            IndexEntry::Vacant(entry) => {
                entry.insert(projection);
            }
        }
    }

    /// Records selection-dependent traversal so it is revisited if the definition is discovered.
    pub fn selection_query(&mut self, selected: SelectedDefinitions<'_>, def_id: DefId) -> bool {
        let included = selected.includes(def_id);
        if !included {
            self.selection_queries.insert(def_id.expect_local());
        }
        included
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TraceState {
    Pending(DefinitionProjection),
    Traced(DefinitionProjection),
}

#[derive(Default)]
struct DefinitionTrace {
    definitions: FxIndexMap<DefPathHash, MetadataDefinition>,
    owners: FxHashMap<DefIndex, DefPathHash>,
    states: FxHashMap<DefIndex, TraceState>,
    pending: VecDeque<LocalDefId>,
    selection_dependents: FxHashMap<LocalDefId, FxIndexSet<TraceScope>>,
    pending_retraces: FxIndexSet<TraceScope>,
}

impl DefinitionTrace {
    fn selected(&self) -> SelectedDefinitions<'_> {
        SelectedDefinitions { definitions: &self.definitions, indices: &self.owners }
    }

    fn observe(
        &mut self,
        def_id: LocalDefId,
        owner: DefPathHash,
        projection: DefinitionProjection,
    ) {
        match self.definitions.entry(owner) {
            IndexEntry::Occupied(mut entry) => {
                let definition = entry.get_mut();
                assert_eq!(
                    definition.def_id, def_id,
                    "one metadata owner selected with two definitions"
                );
                if definition.projection >= projection {
                    return;
                }
                definition.projection = projection;
                let state = self.states.get_mut(&def_id.local_def_index).unwrap();
                match *state {
                    TraceState::Pending(DefinitionProjection::DecodeLayout) => {
                        *state = TraceState::Pending(DefinitionProjection::Semantic);
                    }
                    TraceState::Traced(DefinitionProjection::DecodeLayout) => {
                        *state = TraceState::Pending(DefinitionProjection::Semantic);
                        self.pending.push_back(def_id);
                    }
                    TraceState::Pending(DefinitionProjection::Semantic)
                    | TraceState::Traced(DefinitionProjection::Semantic) => {
                        unreachable!("semantic metadata definition was promoted")
                    }
                }
            }
            IndexEntry::Vacant(entry) => {
                entry.insert(MetadataDefinition { def_id, projection });
                assert!(
                    self.owners.insert(def_id.local_def_index, owner).is_none(),
                    "one metadata definition selected with two owners"
                );
                assert!(
                    self.states
                        .insert(def_id.local_def_index, TraceState::Pending(projection))
                        .is_none(),
                    "metadata definition entered the trace twice"
                );
                self.pending.push_back(def_id);
                if let Some(scopes) = self.selection_dependents.remove(&def_id) {
                    for scope in scopes {
                        self.pending_retraces.insert(scope);
                    }
                }
            }
        }
    }

    fn next(&mut self) -> Option<(LocalDefId, DefinitionProjection)> {
        let def_id = self.pending.pop_front()?;
        let state = self.states.get_mut(&def_id.local_def_index).unwrap();
        let TraceState::Pending(projection) = *state else {
            unreachable!("metadata trace queued a definition that was not pending")
        };
        *state = TraceState::Traced(projection);
        Some((def_id, projection))
    }

    fn drain(
        &mut self,
        owner: impl Fn(LocalDefId) -> DefPathHash,
        mut encode: impl FnMut(TraceScope, SelectedDefinitions<'_>) -> ReferencedDefinitions,
    ) {
        self.trace_scope(TraceScope::Artifact, &owner, &mut encode);

        loop {
            while let Some((def_id, projection)) = self.next() {
                self.trace_scope(
                    TraceScope::Definition { def_id, projection },
                    &owner,
                    &mut encode,
                );
            }

            if let Some(scope) = self.pending_retraces.pop() {
                let scope = match scope {
                    TraceScope::Definition { def_id, projection: _ } => {
                        let owner = self.owners[&def_id.local_def_index];
                        let projection = self.definitions[&owner].projection;
                        TraceScope::Definition { def_id, projection }
                    }
                    TraceScope::Artifact => TraceScope::Artifact,
                    TraceScope::Hygiene => TraceScope::Hygiene,
                };
                self.trace_scope(scope, &owner, &mut encode);
                continue;
            }

            self.trace_scope(TraceScope::Hygiene, &owner, &mut encode);
            if self.pending.is_empty() && self.pending_retraces.is_empty() {
                break;
            }
        }
    }

    fn trace_scope(
        &mut self,
        scope: TraceScope,
        owner: &impl Fn(LocalDefId) -> DefPathHash,
        encode: &mut impl FnMut(TraceScope, SelectedDefinitions<'_>) -> ReferencedDefinitions,
    ) {
        let ReferencedDefinitions { references, selection_queries } =
            encode(scope, self.selected());
        for def_id in selection_queries {
            self.selection_dependents.entry(def_id).or_default().insert(scope);
        }
        for (def_id, projection) in references {
            self.observe(def_id, owner(def_id), projection);
        }
    }
}

/// Maps local definition addresses into the compact address space of one RDR artifact.
///
/// Stable ordering keeps unchanged definitions at the same addresses across artifacts. Parents
/// precede children because compiler algorithms use that property to navigate the definition tree.
/// `DefIndex` remains an address within this single artifact and is never used to establish
/// identity across artifacts.
#[derive(Clone, Debug)]
pub struct MetadataDefinitionLayout {
    definitions: FxIndexMap<DefPathHash, MetadataDefinition>,
    indices: FxHashMap<DefIndex, DefIndex>,
    owners: FxHashMap<DefIndex, DefPathHash>,
    order: Vec<LocalDefId>,
}

/// Describes why a decoded definition table cannot be parsed as an artifact-local layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataDefinitionLayoutError {
    MissingCrateRoot,
    CrateRootNotFirst,
    ParentNotBeforeChild,
    DuplicateOwner,
    DuplicateDefinition,
}

impl MetadataDefinitionLayout {
    /// Traces the transitive definition closure before assigning artifact-local addresses.
    ///
    /// The callback can report references but cannot mutate membership or construct an incomplete
    /// layout. Returning normally is the only completion operation.
    pub fn trace(
        tcx: TyCtxt<'_>,
        semantic_roots: impl IntoIterator<Item = LocalDefId>,
        mut encode: impl FnMut(TraceScope, SelectedDefinitions<'_>) -> ReferencedDefinitions,
    ) -> Self {
        let mut trace = DefinitionTrace::default();
        let observe =
            |trace: &mut DefinitionTrace, def_id: LocalDefId, projection: DefinitionProjection| {
                trace.observe(def_id, tcx.def_path_hash(def_id.to_def_id()), projection);
            };
        observe(&mut trace, CRATE_DEF_ID, DefinitionProjection::Semantic);
        for def_id in semantic_roots {
            observe(&mut trace, def_id, DefinitionProjection::Semantic);
        }

        trace.drain(|def_id| tcx.def_path_hash(def_id.to_def_id()), &mut encode);

        for definition in trace.definitions.values() {
            if definition.def_id == CRATE_DEF_ID {
                continue;
            }
            let parent = tcx.opt_local_parent(definition.def_id);
            assert!(
                parent.is_some_and(|parent| trace.owners.contains_key(&parent.local_def_index)),
                "closed metadata trace omitted the parent of {:?}",
                definition.def_id
            );
        }

        let DefinitionTrace {
            definitions,
            owners,
            states: _,
            pending: _,
            selection_dependents: _,
            pending_retraces: _,
        } = trace;
        let mut definition_ids = Vec::with_capacity(definitions.len());
        for (&hash, &MetadataDefinition { def_id, projection: _ }) in &definitions {
            let mut depth = 0;
            let mut ancestor = def_id;
            while let Some(next) = tcx.opt_local_parent(ancestor) {
                depth += 1;
                ancestor = next;
            }
            definition_ids.push((def_id, hash, depth));
        }
        definition_ids.sort_unstable_by_key(|&(_, hash, depth)| (depth, hash));

        let mut indices =
            FxHashMap::with_capacity_and_hasher(definition_ids.len(), Default::default());
        for (index, &(def_id, _, _)) in definition_ids.iter().enumerate() {
            assert!(
                indices.insert(def_id.local_def_index, DefIndex::from_usize(index)).is_none(),
                "duplicate definition in RDR metadata layout: {def_id:?}"
            );
        }
        let order = definition_ids.into_iter().map(|(def_id, _, _)| def_id).collect();
        Self { definitions, indices, owners, order }
    }

    /// Preserves the compact addresses recorded by an existing metadata artifact.
    ///
    /// Slot zero must be the crate root, parents must precede children, and owners and definitions
    /// must form a bijection. `DefIndex` is local to one artifact, so parsed layouts retain their
    /// encoded order.
    pub fn from_artifact(
        mut definitions: impl ExactSizeIterator<Item = (LocalDefId, DefPathHash, Option<LocalDefId>)>,
    ) -> Result<Self, MetadataDefinitionLayoutError> {
        let definition_count = definitions.len();
        let Some((root, root_owner, root_parent)) = definitions.next() else {
            return Err(MetadataDefinitionLayoutError::MissingCrateRoot);
        };
        if root != CRATE_DEF_ID || root_parent.is_some() {
            return Err(MetadataDefinitionLayoutError::CrateRootNotFirst);
        }

        let mut selected =
            FxIndexMap::with_capacity_and_hasher(definition_count, Default::default());
        selected.insert(
            root_owner,
            MetadataDefinition {
                def_id: CRATE_DEF_ID,
                projection: DefinitionProjection::DecodeLayout,
            },
        );
        let mut indices = FxHashMap::with_capacity_and_hasher(definition_count, Default::default());
        let mut owners = FxHashMap::with_capacity_and_hasher(definition_count, Default::default());
        indices.insert(CRATE_DEF_ID.local_def_index, DefIndex::from_u32(0));
        owners.insert(CRATE_DEF_ID.local_def_index, root_owner);
        let mut order = Vec::with_capacity(definition_count);
        order.push(CRATE_DEF_ID);

        for (artifact_index, (def_id, owner, parent)) in definitions.enumerate() {
            if indices.contains_key(&def_id.local_def_index) {
                return Err(MetadataDefinitionLayoutError::DuplicateDefinition);
            }
            let Some(parent) = parent else {
                return Err(MetadataDefinitionLayoutError::ParentNotBeforeChild);
            };
            if !indices.contains_key(&parent.local_def_index) {
                return Err(MetadataDefinitionLayoutError::ParentNotBeforeChild);
            }
            let IndexEntry::Vacant(entry) = selected.entry(owner) else {
                return Err(MetadataDefinitionLayoutError::DuplicateOwner);
            };
            entry.insert(MetadataDefinition {
                def_id,
                projection: DefinitionProjection::DecodeLayout,
            });
            indices.insert(def_id.local_def_index, DefIndex::from_usize(artifact_index + 1));
            owners.insert(def_id.local_def_index, owner);
            order.push(def_id);
        }

        Ok(Self { definitions: selected, indices, owners, order })
    }

    /// Exposes membership without permitting mutation of a closed layout.
    pub fn selection(&self) -> SelectedDefinitions<'_> {
        SelectedDefinitions { definitions: &self.definitions, indices: &self.owners }
    }

    /// Visits definitions in their artifact-local address order.
    ///
    /// RDR emission must use this order because record occurrences participate in content-derived
    /// hygiene provenance. Re-entering session allocation order would make the closed layout only
    /// partially authoritative.
    pub fn iter(&self) -> impl Iterator<Item = LocalDefId> + '_ {
        self.order.iter().copied()
    }

    /// Maps a session-local definition through this closed artifact layout.
    ///
    /// Panics when encoding discovers a definition that tracing did not include.
    pub fn encode(&self, index: DefIndex) -> DefIndex {
        self.indices
            .get(&index)
            .copied()
            .unwrap_or_else(|| panic!("closed metadata layout omitted definition {index:?}"))
    }
}

#[test]
fn repeated_reference_does_not_duplicate_pending_work() {
    let mut trace = DefinitionTrace::default();
    let def_id = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let owner = DefPathHash(Fingerprint::new(1, 1));

    trace.observe(def_id, owner, DefinitionProjection::DecodeLayout);
    trace.observe(def_id, owner, DefinitionProjection::DecodeLayout);

    assert_eq!(trace.pending.len(), 1);
    assert_eq!(trace.next(), Some((def_id, DefinitionProjection::DecodeLayout)));
    trace.observe(def_id, owner, DefinitionProjection::DecodeLayout);
    assert_eq!(trace.next(), None);
}

#[test]
fn pending_reference_is_promoted_in_place() {
    let mut trace = DefinitionTrace::default();
    let def_id = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let owner = DefPathHash(Fingerprint::new(1, 1));

    trace.observe(def_id, owner, DefinitionProjection::DecodeLayout);
    trace.observe(def_id, owner, DefinitionProjection::Semantic);

    assert_eq!(trace.pending.len(), 1);
    assert_eq!(trace.next(), Some((def_id, DefinitionProjection::Semantic)));
    assert_eq!(trace.next(), None);
}

#[test]
fn traced_reference_is_requeued_once_when_promoted() {
    let mut trace = DefinitionTrace::default();
    let def_id = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let owner = DefPathHash(Fingerprint::new(1, 1));
    let semantic_reference = LocalDefId { local_def_index: DefIndex::from_u32(2) };
    let semantic_owner = DefPathHash(Fingerprint::new(2, 2));

    trace.observe(def_id, owner, DefinitionProjection::DecodeLayout);
    assert_eq!(trace.next(), Some((def_id, DefinitionProjection::DecodeLayout)));
    trace.observe(def_id, owner, DefinitionProjection::Semantic);
    trace.observe(def_id, owner, DefinitionProjection::Semantic);

    assert_eq!(trace.pending.len(), 1);
    assert_eq!(trace.next(), Some((def_id, DefinitionProjection::Semantic)));
    trace.observe(semantic_reference, semantic_owner, DefinitionProjection::Semantic);
    assert_eq!(trace.next(), Some((semantic_reference, DefinitionProjection::Semantic)));
}

#[test]
fn hygiene_promotion_retraces_semantics_and_reopens_definition_processing() {
    let promoted = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let downstream = LocalDefId { local_def_index: DefIndex::from_u32(2) };
    let mut definition_scopes = Vec::new();
    let mut hygiene_scopes = 0;
    let mut trace = DefinitionTrace::default();

    trace.drain(
        |def_id| DefPathHash(Fingerprint::new(u64::from(def_id.local_def_index.as_u32()), 0)),
        |scope, selected| {
            let mut references = ReferencedDefinitions::default();
            match scope {
                TraceScope::Artifact => {
                    references.observe(promoted, DefinitionProjection::DecodeLayout);
                }
                TraceScope::Definition { def_id, projection } => {
                    definition_scopes.push((def_id, projection));
                    if def_id == promoted && projection == DefinitionProjection::Semantic {
                        references.observe(downstream, DefinitionProjection::Semantic);
                    }
                }
                TraceScope::Hygiene => {
                    hygiene_scopes += 1;
                    if hygiene_scopes == 1 {
                        assert_eq!(
                            selected.state(DefPathHash(Fingerprint::new(1, 0))),
                            DefinitionState::DecodeLayout
                        );
                        references.observe(promoted, DefinitionProjection::Semantic);
                    }
                }
            }
            references
        },
    );

    assert_eq!(
        definition_scopes,
        [
            (promoted, DefinitionProjection::DecodeLayout),
            (promoted, DefinitionProjection::Semantic),
            (downstream, DefinitionProjection::Semantic),
        ]
    );
    assert_eq!(hygiene_scopes, 2);
    assert_eq!(
        trace.selected().state(DefPathHash(Fingerprint::new(2, 0))),
        DefinitionState::Semantic
    );
}

#[test]
fn observations_join_to_the_strongest_projection() {
    let def_id = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let mut references = ReferencedDefinitions::default();
    references.observe(def_id, DefinitionProjection::Semantic);
    references.observe(def_id, DefinitionProjection::DecodeLayout);

    assert_eq!(references.references[&def_id], DefinitionProjection::Semantic);
}

#[test]
#[should_panic(expected = "closed metadata layout omitted definition")]
fn closed_layout_rejects_an_untraced_definition() {
    let layout = MetadataDefinitionLayout::from_artifact(
        [(CRATE_DEF_ID, DefPathHash(Fingerprint::new(0, 0)), None)].into_iter(),
    )
    .unwrap();

    layout.encode(DefIndex::from_u32(1));
}

/// Carries the result of one complete projection of the metadata schema.
///
/// Coordinates are intentionally excluded from stable hashing so source movement can replace the
/// position data without invalidating the semantic and decode-layout identities.
#[derive(Debug)]
pub struct MetadataProjection {
    pub contract: MetadataContractHash,
    pub decode_layout: MetadataDecodeLayoutId,
    pub definitions: MetadataDefinitionLayout,
    pub hygiene: HygieneEncodeLayout,
    pub span_layout: MetadataSpanLayout,
}

/// Keeps both definition spans in one coordinate-free incremental dependency.
///
/// Metadata must observe declaration and identifier hygiene together without making either
/// source coordinate part of its semantic contract.
#[derive(Clone, Copy, Debug, StableHash)]
pub struct MetadataDefinitionSpans {
    pub span: Span,
    pub ident: Option<Span>,
}

impl crate::query::erase::Erasable for MetadataDefinitionSpans {
    type Storage = [u8; size_of::<Self>()];
}

/// Keeps source coordinates out of metadata's semantic incremental dependencies.
///
/// The wrapped value still hashes span hygiene. Position traversal reads ordinary query results,
/// so coordinate-only edits regenerate the position data without forcing semantic metadata
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

impl StableHash for MetadataProjection {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.contract.stable_hash(hcx, hasher);
        self.decode_layout.stable_hash(hcx, hasher);
    }
}

/// Identifies the compact-address and span-slot layout expected by a metadata decoder.
///
/// This is separate from the semantic crate hash because equal exported semantics can use
/// incompatible artifact-local addresses.
#[derive(
    Clone,
    Copy,
    Debug,
    Decodable_NoContext,
    Encodable_NoContext,
    Eq,
    Hash,
    PartialEq,
    StableHash
)]
pub struct MetadataDecodeLayoutId(pub Fingerprint);

impl crate::query::erase::Erasable for MetadataDecodeLayoutId {
    type Storage = [u8; size_of::<Self>()];
}

/// Keeps a span occurrence transcript inseparable from the slot range that it addresses.
#[derive(
    Clone,
    Copy,
    Debug,
    Decodable_NoContext,
    Encodable_NoContext,
    Eq,
    Hash,
    PartialEq,
    StableHash
)]
pub struct MetadataSpanLayout {
    pub id: Fingerprint,
    pub slot_count: u32,
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
