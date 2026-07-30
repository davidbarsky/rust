use std::borrow::Borrow;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_abi::FIRST_VARIANT;
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::{FxIndexMap, FxIndexSet, StdEntry as Entry};
use rustc_data_structures::memmap::{Mmap, MmapMut};
use rustc_data_structures::stable_hash::{StableHash, StableHasher};
use rustc_data_structures::svh::Svh;
use rustc_data_structures::sync::{par_for_each_in, par_join};
use rustc_data_structures::temp_dir::MaybeTempDir;
use rustc_data_structures::thousands::usize_with_underscores;
use rustc_data_structures::unord::UnordBag;
use rustc_hir as hir;
use rustc_hir::attrs::{AttributeKind, EncodeCrossCrate, StrippedCfgItemVisibility};
use rustc_hir::def::Namespace;
use rustc_hir::def_id::{CRATE_DEF_ID, LOCAL_CRATE, LocalDefId, LocalDefIdSet, LocalModId};
use rustc_hir::def_path_hash_map::DefPathHashMap;
use rustc_hir::definitions::DefPathData;
use rustc_hir::find_attr;
use rustc_hir_pretty::id_to_string;
use rustc_middle::dep_graph::WorkProductId;
use rustc_middle::metadata::{
    DefinitionProjection, MetadataContractHash, MetadataDecodeLayoutId, MetadataDefinitionLayout,
    MetadataDefinitionSpans, MetadataProjection, MetadataSemantic, Reexport, ReferencedDefinitions,
    SelectedDefinitions, TraceScope,
};
use rustc_middle::middle::dependency_format::Linkage;
use rustc_middle::middle::exported_symbols::ExportedSymbol;
use rustc_middle::mir::interpret;
use rustc_middle::query::{LocalCrate, Providers};
use rustc_middle::traits::specialization_graph;
use rustc_middle::ty::AssocContainer;
use rustc_middle::ty::codec::TyEncoder;
use rustc_middle::ty::fast_reject::{self, TreatParams};
use rustc_middle::{bug, span_bug};
use rustc_serialize::opaque::mem_encoder::MemEncoder;
use rustc_serialize::{Decodable, Decoder, Encodable, Encoder, opaque};
use rustc_session::config::mitigation_coverage::DeniedPartialMitigation;
use rustc_session::config::{CrateType, OptLevel, TargetModifier};
use rustc_span::def_id::CRATE_MOD_ID;
use rustc_span::hygiene::{HygieneDelta, HygieneEncodeContext, HygieneEncodeLayout, HygieneTrace};
use rustc_span::{
    ByteSymbol, ExternalSource, FileName, SourceFile, SpanData, SpanEncoder, StableSourceFileId,
    Symbol, SyntaxContext, sym,
};
use tracing::{debug, instrument, trace};

use crate::diagnostics::{FailCreateFileEncoder, FailWriteFile};
use crate::eii::EiiMapEncodedKeyValue;
use crate::rmeta::*;

pub(crate) struct MetadataProjectionEncoder {
    semantic: StableHasher,
    decode_layout: StableHasher,
}

#[derive(Clone, Copy)]
pub(crate) enum SemanticRecordMembership {
    Included,
    Omitted,
}

impl MetadataProjectionEncoder {
    pub(crate) fn new() -> Self {
        Self { semantic: StableHasher::new(), decode_layout: StableHasher::new() }
    }

    pub(crate) fn enter(&mut self, key: MetadataRecordKey, semantic: SemanticRecordMembership) {
        if key.record.projections().contains(PersistedProjection::Semantic) {
            match semantic {
                SemanticRecordMembership::Included => {
                    self.record_semantic(key, Fingerprint::ZERO);
                }
                SemanticRecordMembership::Omitted => {}
            }
        }
        if key.record.projections().contains(PersistedProjection::DecodeLayout) {
            Self::record(&mut self.decode_layout, key, &[]);
        }
    }

    fn record(hasher: &mut StableHasher, key: MetadataRecordKey, bytes: &[u8]) {
        hasher.write_u8(1);
        key.hash(hasher);
        hasher.write_u8(0);
        hasher.write_usize(bytes.len());
        hasher.write(bytes);
    }

    pub(crate) fn record_semantic(&mut self, key: MetadataRecordKey, value: Fingerprint) {
        trace!(
            record = key.record.name(),
            owner = ?key.owner,
            ?value,
            "projected semantic metadata record"
        );
        Self::record(&mut self.semantic, key, &value.to_le_bytes());
    }

    pub(crate) fn record_wire_bytes(&mut self, key: MetadataRecordKey, bytes: &[u8]) {
        if key.record.projections().contains(PersistedProjection::DecodeLayout) {
            Self::record(&mut self.decode_layout, key, bytes);
        }
    }

    pub(crate) fn finish(self) -> (MetadataContractHash, MetadataDecodeLayoutId) {
        (
            MetadataContractHash(Svh::new(self.semantic.finish())),
            MetadataDecodeLayoutId(self.decode_layout.finish()),
        )
    }
}

enum SourceFileLayout {
    Collecting(FxIndexSet<usize>),
    Frozen(Arc<FxIndexSet<usize>>),
}

impl Default for SourceFileLayout {
    fn default() -> Self {
        Self::Collecting(FxIndexSet::default())
    }
}

impl SourceFileLayout {
    fn freeze(&mut self) {
        let Self::Collecting(source_files) = std::mem::take(self) else {
            bug!("metadata source-file layout was frozen twice");
        };
        *self = Self::Frozen(Arc::new(source_files));
    }
}

enum MetadataEncoding<'a> {
    CoarseFull {
        encoder: opaque::FileEncoder<'static>,
    },
    CoarseStub {
        encoder: opaque::FileEncoder<'static>,
    },
    RdrTrace {
        encoder: MemEncoder,
        selected: SelectedDefinitions<'a>,
        references: ReferencedDefinitions,
        scope: TraceScope,
        hygiene_ctxt: Arc<HygieneEncodeContext>,
        hygiene: &'a HygieneTrace,
    },
    RdrProjection {
        encoder: MemEncoder,
        projections: MetadataProjectionEncoder,
        layout: &'a MetadataDefinitionLayout,
        hygiene: &'a HygieneEncodeLayout,
    },
}

struct MetadataEncoder<'a> {
    encoding: MetadataEncoding<'a>,
    records: Vec<MetadataRecordKey>,
}

impl MetadataEncoder<'_> {
    pub(crate) fn enter(&mut self, key: MetadataRecordKey, semantic: SemanticRecordMembership) {
        self.records.push(key);
        match &mut self.encoding {
            MetadataEncoding::RdrProjection { projections, .. } => projections.enter(key, semantic),
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::CoarseStub { .. }
            | MetadataEncoding::RdrTrace { .. } => {}
        }
    }

    pub(crate) fn leave(&mut self, key: MetadataRecordKey) {
        assert_eq!(self.records.pop(), Some(key), "metadata encoder left a different record");
    }

    pub(crate) fn record_semantic(&mut self, fingerprint: Fingerprint) {
        let key = self.active_record();
        match &mut self.encoding {
            MetadataEncoding::RdrProjection { projections, .. } => {
                projections.record_semantic(key, fingerprint);
            }
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::CoarseStub { .. }
            | MetadataEncoding::RdrTrace { .. } => {}
        }
    }

    pub(crate) fn active_record(&self) -> MetadataRecordKey {
        self.records.last().copied().expect("metadata encoder operated outside a declared record")
    }

    pub(crate) fn position(&self) -> usize {
        match &self.encoding {
            MetadataEncoding::CoarseFull { encoder, .. }
            | MetadataEncoding::CoarseStub { encoder } => encoder.position(),
            MetadataEncoding::RdrTrace { encoder, .. }
            | MetadataEncoding::RdrProjection { encoder, .. } => encoder.position(),
        }
    }

    pub(crate) fn file_handle(&self) -> &File {
        match &self.encoding {
            MetadataEncoding::CoarseFull { encoder, .. }
            | MetadataEncoding::CoarseStub { encoder } => encoder.file(),
            MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => {
                bug!("metadata projection has no output file")
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        match &mut self.encoding {
            MetadataEncoding::CoarseFull { encoder, .. }
            | MetadataEncoding::CoarseStub { encoder } => encoder.flush(),
            MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => {}
        }
    }

    fn record_wire_bytes(&mut self, bytes: &[u8]) {
        match &mut self.encoding {
            MetadataEncoding::RdrProjection { projections, .. } => {
                let key = self
                    .records
                    .last()
                    .copied()
                    .expect("metadata encoder operated outside a declared record");
                projections.record_wire_bytes(key, bytes);
            }
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::CoarseStub { .. }
            | MetadataEncoding::RdrTrace { .. } => {}
        }
    }
}

macro_rules! encoder_methods {
        ($($name:ident($ty:ty);)*) => {
            $(
                fn $name(&mut self, value: $ty) {
                    self.record_wire_bytes(&value.to_le_bytes());
                    match &mut self.encoding {
                        MetadataEncoding::CoarseFull { encoder, .. }
                        | MetadataEncoding::CoarseStub { encoder } => encoder.$name(value),
                        MetadataEncoding::RdrTrace { encoder, .. }
                        | MetadataEncoding::RdrProjection { encoder, .. } => encoder.$name(value),
                    }
                }
            )*
        };
    }

impl Encoder for MetadataEncoder<'_> {
    encoder_methods! {
        emit_usize(usize);
        emit_u128(u128);
        emit_u64(u64);
        emit_u32(u32);
        emit_u16(u16);
        emit_u8(u8);

        emit_isize(isize);
        emit_i128(i128);
        emit_i64(i64);
        emit_i32(i32);
        emit_i16(i16);
    }

    fn emit_raw_bytes(&mut self, bytes: &[u8]) {
        self.record_wire_bytes(bytes);
        match &mut self.encoding {
            MetadataEncoding::CoarseFull { encoder, .. }
            | MetadataEncoding::CoarseStub { encoder } => encoder.emit_raw_bytes(bytes),
            MetadataEncoding::RdrTrace { encoder, .. }
            | MetadataEncoding::RdrProjection { encoder, .. } => encoder.emit_raw_bytes(bytes),
        }
    }
}
impl MetadataEncoding<'_> {
    fn definition_layout(&self) -> Option<&MetadataDefinitionLayout> {
        match self {
            Self::CoarseFull { .. } | Self::CoarseStub { .. } | Self::RdrTrace { .. } => None,
            Self::RdrProjection { layout, .. } => Some(layout),
        }
    }
}

impl MetadataEncoder<'_> {
    fn artifact_kind(&self) -> MetadataArtifactKind {
        match &self.encoding {
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::RdrTrace { .. }
            | MetadataEncoding::RdrProjection { .. } => MetadataArtifactKind::Full,
            MetadataEncoding::CoarseStub { .. } => MetadataArtifactKind::Stub,
        }
    }

    fn is_rdr(&self) -> bool {
        match self.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => false,
            MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => true,
        }
    }

    fn includes_semantics(&self, key: MetadataRecordKey) -> bool {
        match &self.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => true,
            MetadataEncoding::RdrTrace { selected, .. } => key.is_semantic(*selected),
            MetadataEncoding::RdrProjection { layout, .. } => key.is_semantic(layout.selection()),
        }
    }

    fn contains_def_id(&mut self, def_id: DefId) -> bool {
        match &mut self.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => true,
            MetadataEncoding::RdrTrace { selected, references, .. } => {
                references.selection_query(*selected, def_id)
            }
            MetadataEncoding::RdrProjection { layout, .. } => layout.selection().includes(def_id),
        }
    }

    fn contains_module_child(&mut self, child: &ModChild) -> bool {
        let def_id = match child.reexport_chain.first().copied() {
            Some(Reexport::Single(def_id))
            | Some(Reexport::Glob(def_id))
            | Some(Reexport::ExternCrate(def_id)) => Some(def_id),
            Some(Reexport::MacroUse) | Some(Reexport::MacroExport) | None => child.res.opt_def_id(),
        };
        def_id.is_none_or(|def_id| self.contains_def_id(def_id))
    }

    fn contains_trait_impl(&mut self, def_id: LocalDefId, trait_ref: ty::TraitRef<'_>) -> bool {
        let tracing = matches!(self.encoding, MetadataEncoding::RdrTrace { .. });
        if self.contains_def_id(def_id.to_def_id()) {
            return true;
        }
        if !tracing {
            return false;
        }
        if trait_ref.def_id.is_local() && self.contains_def_id(trait_ref.def_id) {
            return true;
        }

        trait_ref.self_ty().walk().filter_map(|arg| arg.as_type()).any(|ty| {
            let def_id = match ty.kind() {
                ty::Adt(def, _) => def.did(),
                ty::Foreign(def_id) => *def_id,
                _ => return false,
            };
            def_id.is_local() && self.contains_def_id(def_id)
        })
    }

    fn encode_def_index(&mut self, index: DefIndex, projection: DefinitionProjection) -> DefIndex {
        match &mut self.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => index,
            MetadataEncoding::RdrTrace { references, .. } => {
                references.observe(LocalDefId { local_def_index: index }, projection);
                index
            }
            MetadataEncoding::RdrProjection { layout, .. } => layout.encode(index),
        }
    }
}

pub(crate) struct EncodeContext<'a, 'tcx> {
    opaque: MetadataEncoder<'a>,
    tcx: TyCtxt<'tcx>,
    feat: &'tcx rustc_feature::Features,
    tables: TableBuilders,
    span_layout: SpanLayout,
    span_occurrence_counts: FxHashMap<MetadataRecordKey, u32>,
    hygiene_occurrence_counts: FxHashMap<(MetadataRecordKey, HygieneReferenceKind), u32>,
    recorded_records: FxIndexSet<PersistedRecord>,

    lazy_state: LazyState,
    span_shorthands: FxHashMap<Span, usize>,
    type_shorthands: FxHashMap<Ty<'tcx>, usize>,
    predicate_shorthands: FxHashMap<ty::PredicateKind<'tcx>, usize>,

    interpret_allocs: FxIndexSet<interpret::AllocId>,

    // This is used to speed up Span encoding.
    // The `usize` is an index into the `MonotonicVec`
    // that stores the `SourceFile`
    source_file_cache: (Arc<SourceFile>, usize),
    source_file_layout: SourceFileLayout,
    hygiene_ctxt: Arc<HygieneEncodeContext>,
    // Used for both `Symbol`s and `ByteSymbol`s.
    symbol_index_table: FxHashMap<u32, usize>,
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum HygieneReferenceKind {
    SyntaxContext,
    Expansion,
}

enum HygieneReference {
    SyntaxContext(SyntaxContext),
    Expansion(rustc_span::hygiene::ExpnId),
}

define_table_writers!();
define_root_writers!();

/// If the current crate is a proc-macro, returns early with `LazyArray::default()`.
/// This is useful for skipping the encoding of things that aren't needed
/// for proc-macro crates.
macro_rules! empty_proc_macro {
    ($self:ident) => {
        if $self.is_proc_macro() {
            return LazyArray::default();
        }
    };
}

macro_rules! encoder_methods {
    ($($name:ident($ty:ty);)*) => {
        $(fn $name(&mut self, value: $ty) {
            self.opaque.$name(value)
        })*
    }
}

impl<'a, 'tcx> Encoder for EncodeContext<'a, 'tcx> {
    encoder_methods! {
        emit_usize(usize);
        emit_u128(u128);
        emit_u64(u64);
        emit_u32(u32);
        emit_u16(u16);
        emit_u8(u8);

        emit_isize(isize);
        emit_i128(i128);
        emit_i64(i64);
        emit_i32(i32);
        emit_i16(i16);
    }

    fn emit_raw_bytes(&mut self, bytes: &[u8]) {
        self.opaque.emit_raw_bytes(bytes);
    }
}

impl<'a, 'tcx, T> Encodable<EncodeContext<'a, 'tcx>> for LazyValue<T> {
    fn encode(&self, e: &mut EncodeContext<'a, 'tcx>) {
        e.emit_lazy_distance(self.position);
    }
}

impl<'a, 'tcx, T> Encodable<EncodeContext<'a, 'tcx>> for LazyArray<T> {
    fn encode(&self, e: &mut EncodeContext<'a, 'tcx>) {
        e.emit_usize(self.num_elems);
        if self.num_elems > 0 {
            e.emit_lazy_distance(self.position)
        }
    }
}

impl<'a, 'tcx, I, T> Encodable<EncodeContext<'a, 'tcx>> for LazyTable<I, T> {
    fn encode(&self, e: &mut EncodeContext<'a, 'tcx>) {
        e.emit_usize(self.width);
        e.emit_usize(self.len);
        e.emit_lazy_distance(self.position);
    }
}

impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for ExpnIndex {
    fn encode(&self, s: &mut EncodeContext<'a, 'tcx>) {
        s.emit_u32(self.as_u32());
    }
}

impl<'a, 'tcx> SpanEncoder for EncodeContext<'a, 'tcx> {
    fn encode_crate_num(&mut self, crate_num: CrateNum) {
        if crate_num != LOCAL_CRATE && self.is_proc_macro() {
            panic!("Attempted to encode non-local CrateNum {crate_num:?} for proc-macro crate");
        }
        self.emit_u32(crate_num.as_u32());
    }

    fn encode_def_index(&mut self, def_index: DefIndex) {
        let def_index = self.map_def_index(def_index);
        self.emit_u32(def_index.as_u32());
    }

    fn encode_def_id(&mut self, def_id: DefId) {
        def_id.krate.encode(self);
        let index = match def_id.as_local() {
            Some(_) => self.map_def_index(def_id.index),
            None => def_id.index,
        };
        self.emit_u32(index.as_u32());
    }

    fn encode_syntax_context(&mut self, syntax_context: SyntaxContext) {
        self.trace_hygiene_reference(HygieneReference::SyntaxContext(syntax_context));
        let hygiene_ctxt = Arc::clone(&self.hygiene_ctxt);
        rustc_span::hygiene::raw_encode_syntax_context(syntax_context, &hygiene_ctxt, self);
    }

    fn encode_expn_id(&mut self, expn_id: ExpnId) {
        self.trace_hygiene_reference(HygieneReference::Expansion(expn_id));
        if expn_id.krate == LOCAL_CRATE {
            // We will only write details for local expansions. Non-local expansions will fetch
            // data from the corresponding crate's metadata.
            // FIXME(#43047) FIXME(#74731) We may eventually want to avoid relying on external
            // metadata from proc-macro crates.
            self.hygiene_ctxt.schedule_expn_data_for_encoding(expn_id);
        }
        expn_id.krate.encode(self);
        self.hygiene_ctxt.expansion_index(expn_id).encode(self);
    }

    fn encode_span(&mut self, span: Span) {
        let record = self.opaque.active_record();
        let ordinal = self.span_occurrence_counts.entry(record).or_default();
        let occurrence = SpanOccurrence { record, ordinal: *ordinal };
        *ordinal =
            ordinal.checked_add(1).expect("cannot encode more than U32_MAX spans per record");
        match &self.opaque.encoding {
            MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => {
                let slot = self.span_layout.push(occurrence);
                if span.is_dummy() {
                    RdrSpanLocation::Dummy
                } else {
                    RdrSpanLocation::Position(slot)
                }
                .encode(self);
                span.ctxt().encode(self);
                return;
            }
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => {}
        }
        match self.span_shorthands.entry(span) {
            Entry::Occupied(o) => {
                // If an offset is smaller than the absolute position, we encode with the offset.
                // This saves space since smaller numbers encode in less bits.
                let last_location = *o.get();
                // This cannot underflow. Metadata is written with increasing position(), so any
                // previously saved offset must be smaller than the current position.
                let offset = self.opaque.position() - last_location;
                if offset < last_location {
                    let needed = bytes_needed(offset);
                    SpanTag::indirect(true, needed as u8).encode(self);
                    let bytes = offset.to_le_bytes();
                    self.emit_raw_bytes(&bytes[..needed]);
                } else {
                    let needed = bytes_needed(last_location);
                    SpanTag::indirect(false, needed as u8).encode(self);
                    let bytes = last_location.to_le_bytes();
                    self.emit_raw_bytes(&bytes[..needed]);
                }
            }
            Entry::Vacant(v) => {
                let position = self.opaque.position();
                v.insert(position);
                // Data is encoded with a SpanTag prefix (see below).
                span.data().encode(self);
            }
        }
    }

    fn encode_symbol(&mut self, sym: Symbol) {
        self.encode_symbol_or_byte_symbol(sym.as_u32(), |this| this.emit_str(sym.as_str()));
    }

    fn encode_byte_symbol(&mut self, byte_sym: ByteSymbol) {
        self.encode_symbol_or_byte_symbol(byte_sym.as_u32(), |this| {
            this.emit_byte_str(byte_sym.as_byte_str())
        });
    }
}

impl EncodeContext<'_, '_> {
    fn trace_hygiene_reference(&mut self, reference: HygieneReference) {
        let trace = match &self.opaque.encoding {
            MetadataEncoding::RdrTrace { scope, hygiene, .. }
                if !matches!(scope, TraceScope::Hygiene) =>
            {
                Some(*hygiene)
            }
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::CoarseStub { .. }
            | MetadataEncoding::RdrTrace { .. }
            | MetadataEncoding::RdrProjection { .. } => None,
        };
        let Some(trace) = trace else {
            return;
        };
        let kind = match reference {
            HygieneReference::SyntaxContext(_) => HygieneReferenceKind::SyntaxContext,
            HygieneReference::Expansion(_) => HygieneReferenceKind::Expansion,
        };
        let record = self.opaque.active_record();
        let ordinal = self.hygiene_occurrence_counts.entry((record, kind)).or_default();
        let occurrence = {
            let mut hasher = StableHasher::new();
            (record, kind, *ordinal).hash(&mut hasher);
            hasher.finish()
        };
        *ordinal = ordinal
            .checked_add(1)
            .expect("cannot encode more than U32_MAX hygiene references per record");
        match reference {
            HygieneReference::SyntaxContext(ctxt) => {
                trace.observe_syntax_context(ctxt, occurrence);
            }
            HygieneReference::Expansion(expn) => trace.observe_expansion(expn, occurrence),
        }
    }
}

fn bytes_needed(n: usize) -> usize {
    (usize::BITS - n.leading_zeros()).div_ceil(u8::BITS) as usize
}

impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for SpanData {
    fn encode(&self, s: &mut EncodeContext<'a, 'tcx>) {
        // Don't serialize any `SyntaxContext`s from a proc-macro crate,
        // since we don't load proc-macro dependencies during serialization.
        // This means that any hygiene information from macros used *within*
        // a proc-macro crate (e.g. invoking a macro that expands to a proc-macro
        // definition) will be lost.
        //
        // This can show up in two ways:
        //
        // 1. Any hygiene information associated with identifier of
        // a proc macro (e.g. `#[proc_macro] pub fn $name`) will be lost.
        // Since proc-macros can only be invoked from a different crate,
        // real code should never need to care about this.
        //
        // 2. Using `Span::def_site` or `Span::mixed_site` will not
        // include any hygiene information associated with the definition
        // site. This means that a proc-macro cannot emit a `$crate`
        // identifier which resolves to one of its dependencies,
        // which also should never come up in practice.
        //
        // Additionally, this affects `Span::parent`, and any other
        // span inspection APIs that would otherwise allow traversing
        // the `SyntaxContexts` associated with a span.
        //
        // None of these user-visible effects should result in any
        // cross-crate inconsistencies (getting one behavior in the same
        // crate, and a different behavior in another crate) due to the
        // limited surface that proc-macros can expose.
        //
        // IMPORTANT: If this is ever changed, be sure to update
        // `rustc_span::hygiene::raw_encode_expn_id` to handle
        // encoding `ExpnData` for proc-macro crates.
        let ctxt = if s.is_proc_macro() { SyntaxContext::root() } else { self.ctxt };

        if self.is_dummy() {
            let tag = SpanTag::new(SpanKind::Partial, ctxt, 0);
            tag.encode(s);
            if tag.context().is_none() {
                ctxt.encode(s);
            }
            return;
        }

        // The Span infrastructure should make sure that this invariant holds:
        debug_assert!(self.lo <= self.hi);

        if !s.source_file_cache.0.contains(self.lo) {
            let source_map = s.tcx.sess.source_map();
            let source_file_index = source_map.lookup_source_file_idx(self.lo);
            s.source_file_cache =
                (Arc::clone(&source_map.files()[source_file_index]), source_file_index);
        }
        let (ref source_file, source_file_index) = s.source_file_cache;
        debug_assert!(source_file.contains(self.lo));

        if !source_file.contains(self.hi) {
            // Unfortunately, macro expansion still sometimes generates Spans
            // that malformed in this way.
            let tag = SpanTag::new(SpanKind::Partial, ctxt, 0);
            tag.encode(s);
            if tag.context().is_none() {
                ctxt.encode(s);
            }
            return;
        }

        // There are two possible cases here:
        // 1. This span comes from a 'foreign' crate - e.g. some crate upstream of the
        // crate we are writing metadata for. When the metadata for *this* crate gets
        // deserialized, the deserializer will need to know which crate it originally came
        // from. We use `TAG_VALID_SPAN_FOREIGN` to indicate that a `CrateNum` should
        // be deserialized after the rest of the span data, which tells the deserializer
        // which crate contains the source map information.
        // 2. This span comes from our own crate. No special handling is needed - we just
        // write `TAG_VALID_SPAN_LOCAL` to let the deserializer know that it should use
        // our own source map information.
        //
        // If we're a proc-macro crate, we always treat this as a local `Span`.
        // In `encode_source_map`, we serialize foreign `SourceFile`s into our metadata
        // if we're a proc-macro crate.
        // This allows us to avoid loading the dependencies of proc-macro crates: all of
        // the information we need to decode `Span`s is stored in the proc-macro crate.
        let (kind, metadata_index) = if source_file.is_imported() && !s.is_proc_macro() {
            // To simplify deserialization, we 'rebase' this span onto the crate it originally came
            // from (the crate that 'owns' the file it references. These rebased 'lo' and 'hi'
            // values are relative to the source map information for the 'foreign' crate whose
            // CrateNum we write into the metadata. This allows `imported_source_files` to binary
            // search through the 'foreign' crate's source map information, using the
            // deserialized 'lo' and 'hi' values directly.
            //
            // All of this logic ensures that the final result of deserialization is a 'normal'
            // Span that can be used without any additional trouble.
            let metadata_index = {
                // Introduce a new scope so that we drop the 'read()' temporary
                match &*source_file.external_src.read() {
                    ExternalSource::Foreign { metadata_index, .. } => *metadata_index,
                    src => panic!("Unexpected external source {src:?}"),
                }
            };

            (SpanKind::Foreign, metadata_index)
        } else {
            let metadata_index = match &mut s.source_file_layout {
                SourceFileLayout::Collecting(source_files) => {
                    source_files.insert_full(source_file_index).0
                }
                SourceFileLayout::Frozen(source_files) => source_files
                    .get_index_of(&source_file_index)
                    .expect("metadata span references a source file absent from its layout"),
            };
            let metadata_index: u32 =
                metadata_index.try_into().expect("cannot export more than U32_MAX files");

            (SpanKind::Local, metadata_index)
        };

        // Encode the start position relative to the file start, so we profit more from the
        // variable-length integer encoding.
        let lo = self.lo - source_file.start_pos;

        // Encode length which is usually less than span.hi and profits more
        // from the variable-length integer encoding that we use.
        let len = self.hi - self.lo;

        let tag = SpanTag::new(kind, ctxt, len.0 as usize);
        tag.encode(s);
        if tag.context().is_none() {
            ctxt.encode(s);
        }
        lo.encode(s);
        if tag.length().is_none() {
            len.encode(s);
        }

        // Encode the index of the `SourceFile` for the span, in order to make decoding faster.
        metadata_index.encode(s);

        if kind == SpanKind::Foreign {
            // This needs to be two lines to avoid holding the `s.source_file_cache`
            // while calling `cnum.encode(s)`
            let cnum = s.source_file_cache.0.cnum;
            cnum.encode(s);
        }
    }
}

impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for [u8] {
    fn encode(&self, e: &mut EncodeContext<'a, 'tcx>) {
        Encoder::emit_usize(e, self.len());
        e.emit_raw_bytes(self);
    }
}

impl<'a, 'tcx> TyEncoder<'tcx> for EncodeContext<'a, 'tcx> {
    const CLEAR_CROSS_CRATE: bool = true;

    fn position(&self) -> usize {
        self.opaque.position()
    }

    fn type_shorthands(&mut self) -> &mut FxHashMap<Ty<'tcx>, usize> {
        &mut self.type_shorthands
    }

    fn predicate_shorthands(&mut self) -> &mut FxHashMap<ty::PredicateKind<'tcx>, usize> {
        &mut self.predicate_shorthands
    }

    fn encode_alloc_id(&mut self, alloc_id: &rustc_middle::mir::interpret::AllocId) {
        let (index, _) = self.interpret_allocs.insert_full(*alloc_id);

        index.encode(self);
    }
}

// Shorthand for `$self.$tables.$table.set_some($def_id.index, $self.lazy($value))`, which would
// normally need extra variables to avoid errors about multiple mutable borrows.
macro_rules! record {
    ($self:ident.$tables:ident.$table:ident[$def_id:expr] <- $value:expr) => {{
        if $self.visits(PersistedRecord::Table(PersistedTable::$table)) {
            $self.$table($def_id.index, $value, |encoder, value| encoder.lazy(value));
        }
    }};
}

// Shorthand for `$self.$tables.$table.set_some($def_id.index, $self.lazy_array($value))`, which would
// normally need extra variables to avoid errors about multiple mutable borrows.
macro_rules! record_array {
    ($self:ident.$tables:ident.$table:ident[$def_id:expr] <- $value:expr) => {{
        if $self.visits(PersistedRecord::Table(PersistedTable::$table)) {
            let values: Vec<_> = $value.into_iter().collect();
            $self.$table($def_id.index, values, |encoder, values| encoder.lazy_array(values));
        }
    }};
}

impl<'a, 'tcx> EncodeContext<'a, 'tcx> {
    fn is_proc_macro(&self) -> bool {
        self.tcx.crate_types().contains(&CrateType::ProcMacro)
    }

    fn visits(&self, record: PersistedRecord) -> bool {
        self.opaque.artifact_kind().contains(record)
    }

    fn encode_preamble(&mut self) {
        self.with_record(
            PersistedRecord::Artifact(PersistedArtifactRecord::metadata_header),
            |encoder| encoder.emit_raw_bytes(METADATA_HEADER),
        );
        self.with_record(
            PersistedRecord::Artifact(PersistedArtifactRecord::root_position),
            |encoder| encoder.emit_raw_bytes(&0u64.to_le_bytes()),
        );
    }

    pub(crate) fn project_semantic<T: MetadataSemanticValue + StableHash + ?Sized>(
        &mut self,
        value: &T,
    ) {
        match &self.opaque.encoding {
            MetadataEncoding::RdrProjection { .. } => {}
            MetadataEncoding::CoarseFull { .. }
            | MetadataEncoding::CoarseStub { .. }
            | MetadataEncoding::RdrTrace { .. } => return,
        }
        let key = self.opaque.active_record();
        if !key.record.projections().contains(PersistedProjection::Semantic)
            || !self.opaque.includes_semantics(key)
        {
            return;
        }
        let layout = self.hygiene_ctxt.artifact_layout();
        let fingerprint = self.tcx.with_metadata_stable_hashing_context(layout, |hcx| {
            let mut hasher = StableHasher::new();
            value.stable_hash(hcx, &mut hasher);
            hasher.finish()
        });
        self.opaque.record_semantic(fingerprint);
    }

    pub(crate) fn map_def_index(&mut self, index: DefIndex) -> DefIndex {
        let key = self.opaque.active_record();
        let projection = if key.record.projections().contains(PersistedProjection::Semantic)
            && self.opaque.includes_semantics(key)
        {
            DefinitionProjection::Semantic
        } else {
            DefinitionProjection::DecodeLayout
        };
        self.opaque.encode_def_index(index, projection)
    }

    pub(crate) fn with_record<T>(
        &mut self,
        record: PersistedRecord,
        encode: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.with_record_key(MetadataRecordKey { record, owner: None }, encode)
    }

    fn with_definition_record(
        &mut self,
        record: PersistedRecord,
        index: DefIndex,
        encode: impl FnOnce(&mut Self),
    ) {
        let def_id = LocalDefId { local_def_index: index };
        let owner = if self.opaque.is_rdr() {
            Some(self.tcx.definitions().def_path_hash(def_id))
        } else {
            None
        };
        let key = MetadataRecordKey { record, owner };
        if let MetadataEncoding::RdrTrace { selected, references, .. } = &mut self.opaque.encoding
            && !references.selection_query(*selected, def_id.to_def_id())
        {
            return;
        }
        self.with_record_key(key, encode);
    }

    fn with_record_key<T>(
        &mut self,
        key: MetadataRecordKey,
        encode: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let semantic = if self.opaque.includes_semantics(key) {
            SemanticRecordMembership::Included
        } else {
            SemanticRecordMembership::Omitted
        };
        self.with_record_key_membership(key, semantic, encode)
    }

    fn with_wire_record<T>(
        &mut self,
        record: PersistedRecord,
        encode: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let projections = record.projections();
        assert!(
            projections.contains(PersistedProjection::Semantic)
                && projections.contains(PersistedProjection::DecodeLayout),
            "wire-only metadata record must have separate semantic and decode projections"
        );
        self.with_record_key_membership(
            MetadataRecordKey { record, owner: None },
            SemanticRecordMembership::Omitted,
            encode,
        )
    }

    fn with_record_key_membership<T>(
        &mut self,
        key: MetadataRecordKey,
        semantic: SemanticRecordMembership,
        encode: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let record = key.record;
        assert!(
            self.visits(record)
                && self
                    .opaque
                    .encoding
                    .definition_layout()
                    .is_none_or(|layout| key.is_selected(layout.selection())),
            "metadata encoder entered a record absent from its selection: {}",
            record.name()
        );
        self.recorded_records.insert(record);
        self.opaque.enter(key, semantic);
        let result = encode(self);
        self.opaque.leave(key);
        result
    }

    pub(crate) fn encode_table<I, T, const N: usize>(
        &mut self,
        record: PersistedTable,
        table: &DeclaredTable<I, T>,
    ) -> LazyTable<I, T>
    where
        I: Idx,
        T: FixedSizeEncoding<ByteArray = [u8; N]>,
    {
        self.with_record(PersistedRecord::Table(record), |encoder| {
            table.encode(encoder.position(), |bytes| encoder.emit_raw_bytes(bytes))
        })
    }

    fn emit_lazy_distance(&mut self, position: NonZero<usize>) {
        let pos = position.get();
        let distance = match self.lazy_state {
            LazyState::NoNode => bug!("emit_lazy_distance: outside of a metadata node"),
            LazyState::NodeStart(start) => {
                let start = start.get();
                assert!(pos <= start);
                start - pos
            }
            LazyState::Previous(last_pos) => {
                assert!(
                    last_pos <= position,
                    "make sure that the calls to `lazy*` \
                     are in the same order as the metadata fields",
                );
                position.get() - last_pos.get()
            }
        };
        self.lazy_state = LazyState::Previous(NonZero::new(pos).unwrap());
        self.emit_usize(distance);
    }

    fn lazy<T: ParameterizedOverTcx, B: Borrow<T::Value<'tcx>>>(&mut self, value: B) -> LazyValue<T>
    where
        T::Value<'tcx>: Encodable<EncodeContext<'a, 'tcx>>,
    {
        let pos = NonZero::new(self.position()).unwrap();

        assert_eq!(self.lazy_state, LazyState::NoNode);
        self.lazy_state = LazyState::NodeStart(pos);
        value.borrow().encode(self);
        self.lazy_state = LazyState::NoNode;

        assert!(pos.get() <= self.position());

        LazyValue::from_position(pos)
    }

    fn lazy_doc_link_resolutions(
        &mut self,
        resolutions: &[DocLinkResolution],
    ) -> LazyValue<DocLinkResMap> {
        let pos = NonZero::new(self.position()).unwrap();

        assert_eq!(self.lazy_state, LazyState::NoNode);
        self.lazy_state = LazyState::NodeStart(pos);
        self.emit_usize(resolutions.len());
        for resolution in resolutions {
            resolution.symbol.encode(self);
            resolution.namespace.encode(self);
            resolution.resolution.encode(self);
        }
        self.lazy_state = LazyState::NoNode;

        assert!(pos.get() <= self.position());

        LazyValue::from_position(pos)
    }

    fn lazy_def_id_map<T: ParameterizedOverTcx>(
        &mut self,
        entries: &[(DefId, T::Value<'tcx>)],
    ) -> LazyValue<DefIdMap<T>>
    where
        T::Value<'tcx>: Encodable<EncodeContext<'a, 'tcx>>,
    {
        let pos = NonZero::new(self.position()).unwrap();

        assert_eq!(self.lazy_state, LazyState::NoNode);
        self.lazy_state = LazyState::NodeStart(pos);
        self.emit_usize(entries.len());
        for (def_id, value) in entries {
            def_id.encode(self);
            value.encode(self);
        }
        self.lazy_state = LazyState::NoNode;

        assert!(pos.get() <= self.position());

        LazyValue::from_position(pos)
    }

    fn lazy_array<T: ParameterizedOverTcx, I: IntoIterator<Item = B>, B: Borrow<T::Value<'tcx>>>(
        &mut self,
        values: I,
    ) -> LazyArray<T>
    where
        T::Value<'tcx>: Encodable<EncodeContext<'a, 'tcx>>,
    {
        let pos = NonZero::new(self.position()).unwrap();

        assert_eq!(self.lazy_state, LazyState::NoNode);
        self.lazy_state = LazyState::NodeStart(pos);
        let len = values.into_iter().map(|value| value.borrow().encode(self)).count();
        self.lazy_state = LazyState::NoNode;

        assert!(pos.get() <= self.position());

        LazyArray::from_position_and_num_elems(pos, len)
    }

    fn encode_symbol_or_byte_symbol(
        &mut self,
        index: u32,
        emit_str_or_byte_str: impl Fn(&mut Self),
    ) {
        self.opaque.active_record();
        // if symbol/byte symbol is predefined, emit tag and symbol index
        if Symbol::is_predefined(index) {
            self.emit_u8(SYMBOL_PREDEFINED);
            self.emit_u32(index);
        } else if let Some(&position) = self.symbol_index_table.get(&index) {
            self.emit_u8(SYMBOL_OFFSET);
            self.emit_usize(position);
        } else {
            self.emit_u8(SYMBOL_STR);
            let position = self.opaque.position();
            self.symbol_index_table.insert(index, position);
            emit_str_or_byte_str(self);
        }
    }

    fn encode_def_path_table(&mut self) {
        if !self.is_proc_macro() {
            return;
        }

        let defs = self.tcx.definitions();
        for def_id in std::iter::once(CRATE_DEF_ID)
            .chain(self.tcx.metadata_resolutions(()).0.proc_macros.iter().copied())
        {
            let def_path_hash = defs.def_path_hash(def_id);
            self.def_keys(
                def_id.local_def_index,
                DefKeyRecord { hash: def_path_hash, key: defs.def_key(def_id) },
                |encoder, record| encoder.lazy(record.key),
            );
            self.def_path_hashes(def_id.local_def_index, def_path_hash, |_, def_path_hash| {
                def_path_hash.local_hash().as_u64()
            });
        }
    }

    fn encode_def_path_hash_map(&mut self) -> LazyValue<DefPathHashMapRef<'static>> {
        self.def_path_hash_map((), |encoder, ()| {
            if encoder.opaque.is_rdr() {
                let definitions = encoder.tcx.definitions();
                let mut entries = definitions
                    .enumerated_keys_and_path_hashes()
                    .filter_map(|(def_index, _, hash)| {
                        let def_id = LocalDefId { local_def_index: def_index };
                        if !encoder.opaque.contains_def_id(def_id.to_def_id()) {
                            return None;
                        }
                        Some((hash, encoder.map_def_index(def_index)))
                    })
                    .collect::<Vec<_>>();
                entries.sort_unstable_by_key(|(hash, _)| *hash);

                let mut map = DefPathHashMap::default();
                for (hash, encoded_index) in entries {
                    assert!(
                        map.insert(&hash.local_hash(), &encoded_index).is_none(),
                        "duplicate exported DefPathHash {hash:?}"
                    );
                }
                encoder.lazy(DefPathHashMapRef::OwnedForEncoding(map))
            } else {
                encoder.lazy(DefPathHashMapRef::BorrowedFromTcx(
                    encoder.tcx.def_path_hash_to_def_index_map(),
                ))
            }
        })
    }

    fn encode_source_map(&mut self) -> LazyTable<u32, Option<LazyValue<rustc_span::SourceFile>>> {
        let source_map = self.tcx.sess.source_map();
        let all_source_files = source_map.files();
        let SourceFileLayout::Frozen(source_file_layout) = &self.source_file_layout else {
            bug!("metadata encoded a source map before freezing its layout");
        };
        let source_file_layout = Arc::clone(source_file_layout);

        let mut adapted = TableBuilder::default();

        let local_crate_stable_id = self.tcx.stable_crate_id(LOCAL_CRATE);

        // Only serialize `SourceFile`s that were used during the encoding of a `Span`.
        //
        // The order in which we encode source files is important here: the on-disk format for
        // `Span` contains the index of the corresponding `SourceFile`.
        for (on_disk_index, &source_file_index) in source_file_layout.iter().enumerate() {
            let source_file = &all_source_files[source_file_index];
            // Don't serialize imported `SourceFile`s, unless we're in a proc-macro crate.
            assert!(!source_file.is_imported() || self.is_proc_macro());

            // At export time we expand all source file paths to absolute paths because
            // downstream compilation sessions can have a different compiler working
            // directory, so relative paths from this or any other upstream crate
            // won't be valid anymore.
            //
            // At this point we also erase the actual on-disk path and only keep
            // the remapped version -- as is necessary for reproducible builds.
            let mut adapted_source_file = (**source_file).clone();

            match source_file.name {
                FileName::Real(ref original_file_name) => {
                    let mut adapted_file_name = original_file_name.clone();
                    adapted_file_name.update_for_crate_metadata();
                    adapted_source_file.name = FileName::Real(adapted_file_name);
                }
                _ => {
                    // expanded code, not from a file
                }
            };

            // We're serializing this `SourceFile` into our crate metadata,
            // so mark it as coming from this crate.
            // This also ensures that we don't try to deserialize the
            // `CrateNum` for a proc-macro dependency - since proc macro
            // dependencies aren't loaded when we deserialize a proc-macro,
            // trying to remap the `CrateNum` would fail.
            if self.is_proc_macro() {
                adapted_source_file.cnum = LOCAL_CRATE;
            }

            // Update the `StableSourceFileId` to make sure it incorporates the
            // id of the current crate. This way it will be unique within the
            // crate graph during downstream compilation sessions.
            adapted_source_file.stable_id = StableSourceFileId::from_filename_for_export(
                &adapted_source_file.name,
                local_crate_stable_id,
            );

            let on_disk_index: u32 =
                on_disk_index.try_into().expect("cannot export more than U32_MAX files");
            adapted.set_some(on_disk_index, self.lazy(adapted_source_file));
        }

        adapted.encode(self.position(), |bytes| self.emit_raw_bytes(bytes))
    }

    fn encode_interpret_alloc_index(&mut self) -> LazyArray<u64> {
        self.interpret_alloc_index((), |encoder, ()| {
            let mut positions = Vec::new();
            let mut encoded = 0;
            trace!("beginning to encode alloc ids");
            loop {
                let pending = encoder.interpret_allocs.len();
                if encoded == pending {
                    break;
                }
                trace!("encoding {} further alloc ids", pending - encoded);
                for index in encoded..pending {
                    let id = encoder.interpret_allocs[index];
                    positions.push(encoder.position() as u64);
                    interpret::specialized_encode_alloc_id(encoder, encoder.tcx, id);
                }
                encoded = pending;
            }
            encoder.lazy_array(positions)
        })
    }

    fn encode_artifact_records(&mut self) {
        let _ = self.encode_externally_implementable_items();
        let _ = self.encode_crate_deps();
        let _ = self.encode_dylib_dependency_formats();
        let _ = self.encode_lib_features();
        let _ = self.encode_stability_implications();
        let _ = self.encode_lang_items();
        let _ = self.encode_lang_items_missing();
        let _ = self.encode_stripped_cfg_items();
        let _ = self.encode_diagnostic_items();
        let _ = self.encode_canonical_symbols();
        let _ = self.encode_native_libraries();
        let _ = self.encode_foreign_modules();
        let _ = self.encode_traits();
        let _ = self.encode_impls();
        let _ = self.encode_incoherent_impls();
        let _ = self.encode_debugger_visualizers();
        let _ = self.encode_exportable_items();
        let _ = self.encode_stable_order_of_exportable_impls();

        let _ = self.encode_exported_symbols();
        let _ = self.encode_def_path_hash_map();
        let _ = self.encode_target_modifiers();
        let _ = self.encode_enabled_denied_partial_mitigations();
    }

    fn encode_exported_symbols(
        &mut self,
    ) -> (
        LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>,
        LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>,
    ) {
        let non_generic = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .exported_non_generic_symbols(LOCAL_CRATE)
                .iter()
                .copied()
                .filter(|(symbol, _)| match symbol {
                    ExportedSymbol::NonGeneric(def_id)
                    | ExportedSymbol::Generic(def_id, _)
                    | ExportedSymbol::AsyncDropGlue(def_id, _)
                    | ExportedSymbol::ThreadLocalShim(def_id) => {
                        self.opaque.contains_def_id(*def_id)
                    }
                    ExportedSymbol::DropGlue(_)
                    | ExportedSymbol::AsyncDropGlueCtorShim(_)
                    | ExportedSymbol::NoDefId(_) => true,
                })
                .collect()
        };
        let generic = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .exported_generic_symbols(LOCAL_CRATE)
                .iter()
                .copied()
                .filter(|(symbol, _)| match symbol {
                    ExportedSymbol::NonGeneric(def_id)
                    | ExportedSymbol::Generic(def_id, _)
                    | ExportedSymbol::AsyncDropGlue(def_id, _)
                    | ExportedSymbol::ThreadLocalShim(def_id) => {
                        self.opaque.contains_def_id(*def_id)
                    }
                    ExportedSymbol::DropGlue(_)
                    | ExportedSymbol::AsyncDropGlueCtorShim(_)
                    | ExportedSymbol::NoDefId(_) => true,
                })
                .collect()
        };
        let non_generic = self.exported_non_generic_symbols(non_generic, |encoder, symbols| {
            encoder.lazy_array(symbols)
        });
        let generic =
            self.exported_generic_symbols(generic, |encoder, symbols| encoder.lazy_array(symbols));
        (non_generic, generic)
    }

    fn encode_crate_root(&mut self) -> (LazyValue<CrateRoot>, HygieneDelta) {
        let contract_hash = match &self.opaque.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => {
                self.tcx.crate_hash(LOCAL_CRATE)
            }
            MetadataEncoding::RdrTrace { .. } => bug!("definition trace encoded a crate root"),
            MetadataEncoding::RdrProjection { .. } => Svh::new(Fingerprint::ZERO),
        };

        let tcx = self.tcx;
        let mut stats: Vec<(&'static str, usize)> = Vec::with_capacity(32);

        macro_rules! stat {
            ($label:literal, $f:expr) => {{
                let orig_pos = self.position();
                let res = $f();
                stats.push(($label, self.position() - orig_pos));
                res
            }};
        }

        // We have already encoded some things. Get their combined size from the current position.
        stats.push(("preamble", self.position()));

        let externally_implementable_items = stat!("externally-implementable-items", || self
            .encode_externally_implementable_items());

        let (crate_deps, dylib_dependency_formats) =
            stat!("dep", || { (self.encode_crate_deps(), self.encode_dylib_dependency_formats()) });

        let lib_features = stat!("lib-features", || self.encode_lib_features());

        let stability_implications =
            stat!("stability-implications", || self.encode_stability_implications());

        let (lang_items, lang_items_missing) = stat!("lang-items", || {
            (self.encode_lang_items(), self.encode_lang_items_missing())
        });

        let stripped_cfg_items = stat!("stripped-cfg-items", || self.encode_stripped_cfg_items());

        let diagnostic_items = stat!("diagnostic-items", || self.encode_diagnostic_items());

        let canonical_symbols = stat!("canonical-symbols", || self.encode_canonical_symbols());

        let native_libraries = stat!("native-libs", || self.encode_native_libraries());

        let foreign_modules = stat!("foreign-modules", || self.encode_foreign_modules());

        _ = stat!("def-path-table", || self.encode_def_path_table());

        // Encode the def IDs of traits, for rustdoc and diagnostics.
        let traits = stat!("traits", || self.encode_traits());

        // Encode the def IDs of impls, for coherence checking.
        let impls = stat!("impls", || self.encode_impls());

        let incoherent_impls = stat!("incoherent-impls", || self.encode_incoherent_impls());

        _ = stat!("def-ids", || self.encode_def_ids());

        let interpret_alloc_index =
            stat!("interpret-alloc-index", || self.encode_interpret_alloc_index());

        // Encode the proc macro data. This affects `tables`, so we need to do this before we
        // encode the tables. This overwrites def_keys, so it must happen after
        // encode_def_path_table.
        let proc_macro_data = stat!("proc-macro-data", || {
            self.proc_macro_data((), |encoder, ()| encoder.encode_proc_macros())
        });

        let tables = std::mem::take(&mut self.tables);
        let tables =
            stat!("tables", || self.tables(tables, |encoder, tables| { tables.encode(encoder) }));

        let debugger_visualizers =
            stat!("debugger-visualizers", || self.encode_debugger_visualizers());

        let exportable_items = stat!("exportable-items", || self.encode_exportable_items());

        let stable_order_of_exportable_impls =
            stat!("exportable-items", || self.encode_stable_order_of_exportable_impls());

        let (exported_non_generic_symbols, exported_generic_symbols) =
            stat!("exported-symbols", || self.encode_exported_symbols());

        // Encode the hygiene data.
        // IMPORTANT: this *must* be the last thing that we encode (other than `SourceMap`). The
        // process of encoding other items (e.g. `optimized_mir`) may cause us to load data from
        // the incremental cache. If this causes us to deserialize a `Span`, then we may load
        // additional `SyntaxContext`s into the global `HygieneData`. Therefore, we need to encode
        // the hygiene data last to ensure that we encode any `SyntaxContext`s that might be used.
        let (syntax_contexts, expn_data, expn_hashes, encoded_hygiene) =
            stat!("hygiene", || self.encode_hygiene());

        let def_path_hash_map = stat!("def-path-hash-map", || self.encode_def_path_hash_map());

        // Encode source_map. This needs to be done last, because encoding `Span`s tells us which
        // `SourceFiles` we actually need to encode.
        let source_map = stat!("source-map", || {
            self.source_map((), |encoder, ()| {
                if encoder.opaque.is_rdr() {
                    TableBuilder::<u32, Option<LazyValue<rustc_span::SourceFile>>>::default()
                        .encode(encoder.position(), |bytes| encoder.emit_raw_bytes(bytes))
                } else {
                    encoder.source_file_layout.freeze();
                    encoder.encode_source_map()
                }
            })
        });
        let target_modifiers = stat!("target-modifiers", || self.encode_target_modifiers());
        let denied_partial_mitigations = stat!("denied-partial-mitigations", || self
            .encode_enabled_denied_partial_mitigations());

        let root = stat!("final", || {
            let attrs = tcx.metadata_attrs(CRATE_DEF_ID).0;
            let is_proc_macro_crate = proc_macro_data.is_some();
            let header = self.header(
                CrateHeaderRecord {
                    triple: tcx.sess.opts.target_triple.clone(),
                    hash: contract_hash,
                    name: tcx.crate_name(LOCAL_CRATE),
                    is_proc_macro_crate,
                    is_stub: false,
                },
                |_, record| CrateHeader {
                    triple: record.triple,
                    hash: record.hash,
                    name: record.name,
                    is_proc_macro_crate: record.is_proc_macro_crate,
                    is_stub: record.is_stub,
                },
            );
            let extra_filename =
                self.extra_filename(tcx.sess.opts.cg.extra_filename.clone(), |_, value| value);
            let stable_crate_id =
                self.stable_crate_id(tcx.stable_crate_id(LOCAL_CRATE), |_, value| value);
            let required_panic_strategy = self
                .required_panic_strategy(tcx.required_panic_strategy(LOCAL_CRATE), |_, value| {
                    value
                });
            let panic_in_drop_strategy = self
                .panic_in_drop_strategy(tcx.sess.opts.unstable_opts.panic_in_drop, |_, value| {
                    value
                });
            let edition = self.edition(tcx.sess.edition(), |_, value| value);
            let has_global_allocator =
                self.has_global_allocator(tcx.has_global_allocator(LOCAL_CRATE), |_, value| value);
            let has_alloc_error_handler = self
                .has_alloc_error_handler(tcx.has_alloc_error_handler(LOCAL_CRATE), |_, value| {
                    value
                });
            let has_panic_handler =
                self.has_panic_handler(tcx.has_panic_handler(LOCAL_CRATE), |_, value| value);
            let has_default_lib_allocator = self
                .has_default_lib_allocator(find_attr!(attrs, DefaultLibAllocator), |_, value| {
                    value
                });
            let compiler_builtins =
                self.compiler_builtins(find_attr!(attrs, CompilerBuiltins), |_, value| value);
            let needs_allocator =
                self.needs_allocator(find_attr!(attrs, NeedsAllocator), |_, value| value);
            let needs_panic_runtime =
                self.needs_panic_runtime(find_attr!(attrs, NeedsPanicRuntime), |_, value| value);
            let no_builtins = self.no_builtins(find_attr!(attrs, NoBuiltins), |_, value| value);
            let panic_runtime =
                self.panic_runtime(find_attr!(attrs, PanicRuntime), |_, value| value);
            let profiler_runtime =
                self.profiler_runtime(find_attr!(attrs, ProfilerRuntime), |_, value| value);
            let symbol_mangling_version = self.symbol_mangling_version(
                tcx.sess.opts.get_symbol_mangling_version(),
                |_, value| value,
            );
            let specialization_enabled_in = self.specialization_enabled_in(
                tcx.specialization_enabled_in(LOCAL_CRATE),
                |_, value| value,
            );
            self.lazy(CrateRoot {
                header,
                extra_filename,
                stable_crate_id,
                required_panic_strategy,
                panic_in_drop_strategy,
                edition,
                has_global_allocator,
                has_alloc_error_handler,
                has_panic_handler,
                has_default_lib_allocator,
                externally_implementable_items,
                proc_macro_data,
                debugger_visualizers,
                compiler_builtins,
                needs_allocator,
                needs_panic_runtime,
                no_builtins,
                panic_runtime,
                profiler_runtime,
                symbol_mangling_version,

                crate_deps,
                dylib_dependency_formats,
                lib_features,
                stability_implications,
                lang_items,
                diagnostic_items,
                canonical_symbols,
                lang_items_missing,
                stripped_cfg_items,
                native_libraries,
                foreign_modules,
                source_map,
                target_modifiers,
                denied_partial_mitigations,
                traits,
                impls,
                incoherent_impls,
                exportable_items,
                stable_order_of_exportable_impls,
                exported_non_generic_symbols,
                exported_generic_symbols,
                interpret_alloc_index,
                tables,
                syntax_contexts,
                expn_data,
                expn_hashes,
                def_path_hash_map,
                specialization_enabled_in,
            })
        });

        let total_bytes = self.position();

        let computed_total_bytes: usize = stats.iter().map(|(_, size)| size).sum();
        assert_eq!(total_bytes, computed_total_bytes);

        let emitted_artifact = match &self.opaque.encoding {
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => true,
            MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => false,
        };
        if tcx.sess.opts.unstable_opts.meta_stats && emitted_artifact {
            use std::fmt::Write;

            self.opaque.flush();

            // Rewind and re-read all the metadata to count the zero bytes we wrote.
            let pos_before_rewind = self.opaque.file_handle().stream_position().unwrap();
            let mut zero_bytes = 0;
            self.opaque.file_handle().rewind().unwrap();
            let file = std::io::BufReader::new(self.opaque.file_handle());
            for e in file.bytes() {
                if e.unwrap() == 0 {
                    zero_bytes += 1;
                }
            }
            assert_eq!(self.opaque.file_handle().stream_position().unwrap(), pos_before_rewind);

            stats.sort_by_key(|&(_, usize)| usize);
            stats.reverse(); // bigger items first

            let prefix = "meta-stats";
            let perc = |bytes| (bytes * 100) as f64 / total_bytes as f64;

            let section_w = 23;
            let size_w = 10;
            let banner_w = 64;

            // We write all the text into a string and print it with a single
            // `eprint!`. This is an attempt to minimize interleaved text if multiple
            // rustc processes are printing macro-stats at the same time (e.g. with
            // `RUSTFLAGS='-Zmeta-stats' cargo build`). It still doesn't guarantee
            // non-interleaving, though.
            let mut s = String::new();
            _ = writeln!(s, "{prefix} {}", "=".repeat(banner_w));
            _ = writeln!(s, "{prefix} METADATA STATS: {}", tcx.crate_name(LOCAL_CRATE));
            _ = writeln!(s, "{prefix} {:<section_w$}{:>size_w$}", "Section", "Size");
            _ = writeln!(s, "{prefix} {}", "-".repeat(banner_w));
            for (label, size) in stats {
                _ = writeln!(
                    s,
                    "{prefix} {:<section_w$}{:>size_w$} ({:4.1}%)",
                    label,
                    usize_with_underscores(size),
                    perc(size)
                );
            }
            _ = writeln!(s, "{prefix} {}", "-".repeat(banner_w));
            _ = writeln!(
                s,
                "{prefix} {:<section_w$}{:>size_w$} (of which {:.1}% are zero bytes)",
                "Total",
                usize_with_underscores(total_bytes),
                perc(zero_bytes)
            );
            _ = writeln!(s, "{prefix} {}", "=".repeat(banner_w));
            eprint!("{s}");
        }

        (root, encoded_hygiene)
    }
}

struct AnalyzeAttrState {
    is_exported: bool,
    is_doc_hidden: bool,
}

/// Returns whether an attribute needs to be recorded in metadata, that is, if it's usable and
/// useful in downstream crates. Local-only attributes are an obvious example, but some
/// rustdoc-specific attributes can equally be of use while documenting the current crate only.
///
/// Removing these superfluous attributes speeds up compilation by making the metadata smaller.
///
/// Note: the `is_exported` parameter is used to cache whether the given `DefId` has a public
/// visibility: this is a piece of data that can be computed once per defid, and not once per
/// attribute. Some attributes would only be usable downstream if they are public.
#[inline]
fn analyze_attr(attr: &hir::Attribute, state: &mut AnalyzeAttrState) -> bool {
    let mut should_encode = false;
    if let hir::Attribute::Parsed(p) = attr
        && p.encode_cross_crate() == EncodeCrossCrate::No
    {
        // Attributes not marked encode-cross-crate don't need to be encoded for downstream crates.
    } else if let Some(name) = attr.name()
        && [sym::warn, sym::allow, sym::expect, sym::forbid, sym::deny].contains(&name)
    {
        // Lint attributes don't need to be encoded for downstream crates.
        // FIXME remove this when #152369 is re-merged
    } else if let hir::Attribute::Parsed(AttributeKind::DocComment { .. }) = attr {
        // We keep all doc comments reachable to rustdoc because they might be "imported" into
        // downstream crates if they use `#[doc(inline)]` to copy an item's documentation into
        // their own.
        if state.is_exported {
            should_encode = true;
        }
    } else if let hir::Attribute::Parsed(AttributeKind::Doc(d)) = attr {
        should_encode = true;
        if d.hidden.is_some() {
            state.is_doc_hidden = true;
        }
    } else {
        should_encode = true;
    }
    should_encode
}

fn should_encode_span(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Mod
        | DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::LifetimeParam
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { .. }
        | DefKind::Ctor(..)
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::Macro(_)
        | DefKind::ExternCrate
        | DefKind::Use
        | DefKind::AnonConst
        | DefKind::OpaqueTy
        | DefKind::Field
        | DefKind::Impl { .. }
        | DefKind::Closure
        | DefKind::SyntheticCoroutineBody => true,
        DefKind::ForeignMod | DefKind::GlobalAsm => false,
    }
}

fn should_encode_attrs(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Mod
        | DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { nested: false, .. }
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::Macro(_)
        | DefKind::Field
        | DefKind::ConstParam
        | DefKind::Impl { .. } => true,
        // Encoding attrs for `Use` items allows `#[doc(hidden)]` on re-exports
        // to be read cross-crate, which is needed for diagnostic path selection
        // in `visible_parent_map`. See #153477.
        DefKind::Use => true,
        // Tools may want to be able to detect their tool lints on
        // closures from upstream crates, too. This is used by
        // https://github.com/model-checking/kani and is not a performance
        // or maintenance issue for us.
        DefKind::Closure => true,
        DefKind::SyntheticCoroutineBody => false,
        DefKind::TyParam
        | DefKind::Ctor(..)
        | DefKind::ExternCrate
        | DefKind::ForeignMod
        | DefKind::AnonConst
        | DefKind::OpaqueTy
        | DefKind::LifetimeParam
        | DefKind::Static { nested: true, .. }
        | DefKind::GlobalAsm => false,
    }
}

fn should_encode_expn_that_defined(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Mod
        | DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::Impl { .. } => true,
        DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::TyParam
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::ConstParam
        | DefKind::Static { .. }
        | DefKind::Ctor(..)
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::Macro(_)
        | DefKind::ExternCrate
        | DefKind::Use
        | DefKind::ForeignMod
        | DefKind::AnonConst
        | DefKind::OpaqueTy
        | DefKind::Field
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::Closure
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_visibility(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Mod
        | DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { nested: false, .. }
        | DefKind::Ctor(..)
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::Macro(..)
        | DefKind::Field => true,
        DefKind::Use
        | DefKind::ForeignMod
        | DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::LifetimeParam
        | DefKind::AnonConst
        | DefKind::Static { nested: true, .. }
        | DefKind::OpaqueTy
        | DefKind::GlobalAsm
        | DefKind::Impl { .. }
        | DefKind::Closure
        | DefKind::ExternCrate
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_stability(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Mod
        | DefKind::Ctor(..)
        | DefKind::Variant
        | DefKind::Field
        | DefKind::Struct
        | DefKind::AssocTy
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::Static { .. }
        | DefKind::Const { .. }
        | DefKind::Fn
        | DefKind::ForeignMod
        | DefKind::TyAlias
        | DefKind::OpaqueTy
        | DefKind::Enum
        | DefKind::Union
        | DefKind::Impl { .. }
        | DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Macro(..)
        | DefKind::ForeignTy => true,
        DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::AnonConst
        | DefKind::GlobalAsm
        | DefKind::Closure
        | DefKind::ExternCrate
        | DefKind::SyntheticCoroutineBody => false,
    }
}

/// Whether we should encode MIR. Return a pair, resp. for CTFE and for LLVM.
///
/// Computing, optimizing and encoding the MIR is a relatively expensive operation.
/// We want to avoid this work when not required. Therefore:
/// - we only compute `mir_for_ctfe` on items with const-eval semantics;
/// - we skip `optimized_mir` for check runs.
/// - we only encode `optimized_mir` that could be generated in other crates, that is, a code that
///   is either generic or has inline hint, and is reachable from the other crates (contained
///   in reachable set).
///
/// Note: Reachable set describes definitions that might be generated or referenced from other
/// crates and it can be used to limit optimized MIR that needs to be encoded. On the other hand,
/// the reachable set doesn't have much to say about which definitions might be evaluated at compile
/// time in other crates, so it cannot be used to omit CTFE MIR. For example, `f` below is
/// unreachable and yet it can be evaluated in other crates:
///
/// ```
/// const fn f() -> usize { 0 }
/// pub struct S { pub a: [usize; f()] }
/// ```
fn should_encode_mir(
    tcx: TyCtxt<'_>,
    reachable_set: &LocalDefIdSet,
    def_id: LocalDefId,
) -> (bool, bool) {
    match tcx.def_kind(def_id) {
        // instance_mir uses mir_for_ctfe rather than optimized_mir for constructors
        DefKind::Ctor(_, _) => (true, false),
        // Constants
        DefKind::AnonConst | DefKind::AssocConst { .. } | DefKind::Const { .. } => (true, false),
        // Coroutines require optimized MIR to compute layout.
        DefKind::Closure if tcx.is_coroutine(def_id.to_def_id()) => (false, true),
        DefKind::SyntheticCoroutineBody => (false, true),
        // Full-fledged functions + closures
        DefKind::AssocFn | DefKind::Fn | DefKind::Closure => {
            let opt = tcx.sess.opts.unstable_opts.always_encode_mir
                || (tcx.sess.opts.output_types.should_codegen()
                    && reachable_set.contains(&def_id)
                    && (tcx.generics_of(def_id).requires_monomorphization(tcx)
                        || tcx.cross_crate_inlinable(def_id)));
            // Comptime fns do not have optimized MIR at all.
            let opt =
                opt && !matches!(tcx.constness(def_id), hir::Constness::Const { always: true });
            // The function has a `const` modifier or is in a `const trait`.
            let is_const_fn = tcx.is_const_fn(def_id.to_def_id());
            (is_const_fn, opt)
        }
        // The others don't have MIR.
        _ => (false, false),
    }
}

fn metadata_projection(tcx: TyCtxt<'_>, _: ()) -> MetadataProjection {
    let reachable_set = tcx.reachable_set(());
    let effective_visibilities = tcx.effective_visibilities(());
    let macro_reachable_imports = &tcx.resolutions(()).macro_reachability.imports;
    let mut semantic_roots = Vec::new();
    for def_id in tcx.iter_local_def_id() {
        if def_id == CRATE_DEF_ID {
            continue;
        }
        let def_kind = tcx.def_kind(def_id);
        let inherent_impl = def_kind == (DefKind::Impl { of_trait: false });
        // A trait member can be codegen-reachable merely because its trait is external. Its
        // enclosing impl determines whether that member can affect a downstream crate.
        let trait_impl_is_relevant =
            tcx.trait_impl_of_assoc(def_id.to_def_id()).is_none_or(|impl_id| {
                let impl_id = impl_id.expect_local();
                reachable_set.contains(&impl_id) || effective_visibilities.is_reachable(impl_id)
            });
        // Codegen reachability includes generated coroutine definitions reached through private
        // owners. They become metadata definitions only when an already-selected semantic record
        // references them.
        let reachable = reachable_set.contains(&def_id)
            && def_kind != DefKind::Closure
            && def_kind != DefKind::SyntheticCoroutineBody
            && !inherent_impl
            && trait_impl_is_relevant;
        let effectively_visible =
            effective_visibilities.is_reachable(def_id) && !inherent_impl && trait_impl_is_relevant;
        if reachable || effectively_visible || macro_reachable_imports.contains(&def_id) {
            semantic_roots.push(def_id);
        }
    }
    for &(symbol, _) in tcx
        .exported_non_generic_symbols(LOCAL_CRATE)
        .iter()
        .chain(tcx.exported_generic_symbols(LOCAL_CRATE).iter())
    {
        let def_id = match symbol {
            ExportedSymbol::NonGeneric(def_id)
            | ExportedSymbol::Generic(def_id, _)
            | ExportedSymbol::AsyncDropGlue(def_id, _)
            | ExportedSymbol::ThreadLocalShim(def_id) => def_id,
            ExportedSymbol::DropGlue(_)
            | ExportedSymbol::AsyncDropGlueCtorShim(_)
            | ExportedSymbol::NoDefId(_) => continue,
        };
        if let Some(def_id) = def_id.as_local() {
            semantic_roots.push(def_id);
        }
    }

    let hygiene_ctxt = Arc::new(HygieneEncodeContext::default());
    let (definitions, hygiene) = tcx.with_stable_hashing_context(|mut hcx| {
        HygieneEncodeLayout::trace(&mut hcx, |hygiene| {
            MetadataDefinitionLayout::trace(tcx, semantic_roots, |scope, selected| {
                let mut sink = MemEncoder::new();
                sink.emit_u8(0);
                let mut encoder = EncodeContext::new(
                    tcx,
                    MetadataEncoding::RdrTrace {
                        encoder: sink,
                        selected,
                        references: ReferencedDefinitions::default(),
                        scope,
                        hygiene_ctxt: Arc::clone(&hygiene_ctxt),
                        hygiene,
                    },
                );
                match scope {
                    TraceScope::Artifact => {
                        encoder.encode_artifact_records();
                        let _ = encoder.encode_interpret_alloc_index();
                    }
                    TraceScope::Definition { def_id, projection: _ } => {
                        encoder.encode_definition(def_id);
                        let _ = encoder.encode_interpret_alloc_index();
                    }
                    TraceScope::Hygiene => {
                        let (_, _, _, delta) = encoder.encode_hygiene();
                        hygiene.extend(delta);
                    }
                }
                let MetadataEncoder {
                    encoding: MetadataEncoding::RdrTrace { references, .. },
                    records: _,
                } = encoder.opaque
                else {
                    unreachable!("definition trace changed encoding mode")
                };
                references
            })
        })
    });
    debug!("planned RDR metadata definition and hygiene closure");
    let mut encoder = EncodeContext::new(
        tcx,
        MetadataEncoding::RdrProjection {
            encoder: MemEncoder::new(),
            projections: MetadataProjectionEncoder::new(),
            layout: &definitions,
            hygiene: &hygiene,
        },
    );
    encoder.encode_preamble();
    encoder.with_record(
        PersistedRecord::Artifact(PersistedArtifactRecord::rustc_version),
        |encoder| rustc_version(tcx.sess.cfg_version).encode(encoder),
    );
    let (_, encoded_hygiene) = encoder.encode_crate_root();
    let span_layout = encoder.span_layout.metadata_layout();
    let MetadataEncoder {
        encoding: MetadataEncoding::RdrProjection { projections, .. },
        records: _,
    } = encoder.opaque
    else {
        unreachable!("closed projection changed encoding mode")
    };
    let (contract, decode_layout) = projections.finish();
    hygiene.assert_reached(encoded_hygiene);
    MetadataProjection { contract, decode_layout, definitions, hygiene, span_layout }
}

fn metadata_contract_hash(tcx: TyCtxt<'_>, _: ()) -> MetadataContractHash {
    if !tcx.sess.opts.unstable_opts.rdr || tcx.crate_types().contains(&CrateType::ProcMacro) {
        return MetadataContractHash(tcx.crate_hash(LOCAL_CRATE));
    }
    tcx.metadata_projection(()).contract
}

fn metadata_decode_layout_id(tcx: TyCtxt<'_>, _: LocalCrate) -> MetadataDecodeLayoutId {
    if !tcx.sess.opts.unstable_opts.rdr || tcx.crate_types().contains(&CrateType::ProcMacro) {
        let hash = tcx.crate_hash(LOCAL_CRATE).as_u128();
        return MetadataDecodeLayoutId(Fingerprint::new(hash as u64, (hash >> 64) as u64));
    }
    tcx.metadata_projection(()).decode_layout
}

fn metadata_definition_spans(
    tcx: TyCtxt<'_>,
    def_id: LocalDefId,
) -> MetadataSemantic<MetadataDefinitionSpans> {
    MetadataSemantic(MetadataDefinitionSpans {
        span: tcx.def_span(def_id),
        ident: tcx.def_ident_span(def_id.to_def_id()),
    })
}

fn metadata_resolutions(tcx: TyCtxt<'_>, _: ()) -> MetadataSemantic<&ty::ResolverGlobalCtxt> {
    MetadataSemantic(tcx.resolutions(()))
}

fn metadata_attrs(tcx: TyCtxt<'_>, def_id: LocalDefId) -> MetadataSemantic<&[hir::Attribute]> {
    MetadataSemantic(tcx.hir_attrs(tcx.local_def_id_to_hir_id(def_id)))
}

fn should_encode_variances<'tcx>(tcx: TyCtxt<'tcx>, def_id: DefId, def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::OpaqueTy
        | DefKind::Fn
        | DefKind::Ctor(..)
        | DefKind::AssocFn => true,
        DefKind::AssocTy => {
            // Only encode variances for RPITITs (for traits)
            matches!(tcx.opt_rpitit_info(def_id), Some(ty::ImplTraitInTraitData::Trait { .. }))
        }
        DefKind::Mod
        | DefKind::Variant
        | DefKind::Field
        | DefKind::AssocConst { .. }
        | DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::Static { .. }
        | DefKind::Const { .. }
        | DefKind::ForeignMod
        | DefKind::TyAlias
        | DefKind::Impl { .. }
        | DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Macro(..)
        | DefKind::ForeignTy
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::AnonConst
        | DefKind::GlobalAsm
        | DefKind::Closure
        | DefKind::ExternCrate
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_generics(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { .. }
        | DefKind::Ctor(..)
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::AnonConst
        | DefKind::OpaqueTy
        | DefKind::Impl { .. }
        | DefKind::Field
        | DefKind::TyParam
        | DefKind::Closure
        | DefKind::SyntheticCoroutineBody => true,
        DefKind::Mod
        | DefKind::ForeignMod
        | DefKind::ConstParam
        | DefKind::Macro(..)
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::ExternCrate => false,
    }
}

fn should_encode_type(tcx: TyCtxt<'_>, def_id: LocalDefId, def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Ctor(..)
        | DefKind::Field
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { nested: false, .. }
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::Impl { .. }
        | DefKind::AssocFn
        | DefKind::AssocConst { .. }
        | DefKind::Closure
        | DefKind::ConstParam
        | DefKind::AnonConst
        | DefKind::SyntheticCoroutineBody => true,

        DefKind::OpaqueTy => {
            let origin = tcx.local_opaque_ty_origin(def_id);
            if let hir::OpaqueTyOrigin::FnReturn { parent, .. }
            | hir::OpaqueTyOrigin::AsyncFn { parent, .. } = origin
                && let hir::Node::TraitItem(trait_item) = tcx.hir_node_by_def_id(parent)
                && let (_, hir::TraitFn::Required(..)) = trait_item.expect_fn()
            {
                false
            } else {
                true
            }
        }

        DefKind::AssocTy => {
            let assoc_item = tcx.associated_item(def_id);
            match assoc_item.container {
                ty::AssocContainer::InherentImpl | ty::AssocContainer::TraitImpl(_) => true,
                ty::AssocContainer::Trait => assoc_item.defaultness(tcx).has_value(),
            }
        }
        DefKind::TyParam => {
            let hir::Node::GenericParam(param) = tcx.hir_node_by_def_id(def_id) else { bug!() };
            let hir::GenericParamKind::Type { default, .. } = param.kind else { bug!() };
            default.is_some()
        }

        DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Mod
        | DefKind::ForeignMod
        | DefKind::Macro(..)
        | DefKind::Static { nested: true, .. }
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::ExternCrate => false,
    }
}

fn should_encode_fn_sig(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Fn | DefKind::AssocFn | DefKind::Ctor(_, CtorKind::Fn) => true,

        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Field
        | DefKind::Const { .. }
        | DefKind::Static { .. }
        | DefKind::Ctor(..)
        | DefKind::TyAlias
        | DefKind::OpaqueTy
        | DefKind::ForeignTy
        | DefKind::Impl { .. }
        | DefKind::AssocConst { .. }
        | DefKind::Closure
        | DefKind::ConstParam
        | DefKind::AnonConst
        | DefKind::AssocTy
        | DefKind::TyParam
        | DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Mod
        | DefKind::ForeignMod
        | DefKind::Macro(..)
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::ExternCrate
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_constness(def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Fn
        | DefKind::AssocFn
        | DefKind::Closure
        | DefKind::Ctor(_, CtorKind::Fn)
        | DefKind::Impl { of_trait: false } => true,

        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Field
        | DefKind::Const { .. }
        | DefKind::AssocConst { .. }
        | DefKind::AnonConst
        | DefKind::Static { .. }
        | DefKind::TyAlias
        | DefKind::OpaqueTy
        | DefKind::Impl { .. }
        | DefKind::ForeignTy
        | DefKind::ConstParam
        | DefKind::AssocTy
        | DefKind::TyParam
        | DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Mod
        | DefKind::ForeignMod
        | DefKind::Macro(..)
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::ExternCrate
        | DefKind::Ctor(_, CtorKind::Const)
        | DefKind::Variant
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_const(def_kind: DefKind) -> bool {
    match def_kind {
        // FIXME(mgca): should we remove Const and AssocConst here?
        DefKind::Const { .. } | DefKind::AssocConst { .. } | DefKind::AnonConst => true,

        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Ctor(..)
        | DefKind::Field
        | DefKind::Fn
        | DefKind::Static { .. }
        | DefKind::TyAlias
        | DefKind::OpaqueTy
        | DefKind::ForeignTy
        | DefKind::Impl { .. }
        | DefKind::AssocFn
        | DefKind::Closure
        | DefKind::ConstParam
        | DefKind::AssocTy
        | DefKind::TyParam
        | DefKind::Trait
        | DefKind::TraitAlias
        | DefKind::Mod
        | DefKind::ForeignMod
        | DefKind::Macro(..)
        | DefKind::Use
        | DefKind::LifetimeParam
        | DefKind::GlobalAsm
        | DefKind::ExternCrate
        | DefKind::SyntheticCoroutineBody => false,
    }
}

fn should_encode_const_of_item<'tcx>(tcx: TyCtxt<'tcx>, def_id: DefId, def_kind: DefKind) -> bool {
    // AssocConst ==> assoc item has value
    tcx.is_type_const(def_id)
        && (!matches!(def_kind, DefKind::AssocConst { .. }) || assoc_item_has_value(tcx, def_id))
}

fn assoc_item_has_value<'tcx>(tcx: TyCtxt<'tcx>, def_id: DefId) -> bool {
    let assoc_item = tcx.associated_item(def_id);
    match assoc_item.container {
        ty::AssocContainer::InherentImpl | ty::AssocContainer::TraitImpl(_) => true,
        ty::AssocContainer::Trait => assoc_item.defaultness(tcx).has_value(),
    }
}

impl<'a, 'tcx> EncodeContext<'a, 'tcx> {
    fn encode_attrs(&mut self, def_id: LocalDefId) {
        let tcx = self.tcx;
        let mut state = AnalyzeAttrState {
            is_exported: tcx.effective_visibilities(()).is_exported(def_id),
            is_doc_hidden: false,
        };
        let attr_iter =
            tcx.metadata_attrs(def_id).0.iter().filter(|attr| analyze_attr(*attr, &mut state));

        record_array!(self.tables.attributes[def_id.to_def_id()] <- attr_iter);

        self.attr_flags(def_id.local_def_index, state.is_doc_hidden, |_, is_doc_hidden| {
            if is_doc_hidden { AttrFlags::IS_DOC_HIDDEN } else { AttrFlags::empty() }
        });
    }

    fn encode_def_ids(&mut self) {
        // Proc-macro crates only export proc-macro items, which are looked
        // up using `proc_macro_data`
        if self.is_proc_macro() {
            self.encode_info_for_mod(CRATE_DEF_ID);
            return;
        }

        let tcx = self.tcx;
        let definitions: Vec<_> = match &self.opaque.encoding {
            MetadataEncoding::RdrProjection { layout, .. } => layout.iter().collect(),
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => {
                tcx.iter_local_def_id().collect()
            }
            MetadataEncoding::RdrTrace { .. } => Vec::new(),
        };
        for local_id in definitions {
            self.encode_definition(local_id);
        }
    }

    fn encode_definition(&mut self, local_id: LocalDefId) {
        let tcx = self.tcx;
        let def_kind = tcx.def_kind(local_id);

        // The `DefCollector` will sometimes create unnecessary `DefId`s
        // for trivial const arguments which are directly lowered to
        // `ConstArgKind::Path`. We never actually access this `DefId`
        // anywhere so we don't need to encode it for other crates.
        // FIXME(mgca): This probably isn't true, they probably are accessed, but, test case?
        if def_kind == DefKind::AnonConst
            && matches!(tcx.hir_node_by_def_id(local_id), hir::Node::ConstArg(_))
        {
            return;
        }

        let def_id = local_id.to_def_id();
        let definitions = tcx.definitions();
        let def_path_hash = definitions.def_path_hash(local_id);
        self.def_keys(
            local_id.local_def_index,
            DefKeyRecord { hash: def_path_hash, key: definitions.def_key(local_id) },
            |encoder, record| encoder.lazy(record.key),
        );
        self.def_path_hashes(local_id.local_def_index, def_path_hash, |_, def_path_hash| {
            def_path_hash.local_hash().as_u64()
        });
        self.def_kind(def_id.index, def_kind, |_, def_kind| def_kind);

        if def_kind == DefKind::Field
            && let hir::Node::Field(field) = tcx.hir_node_by_def_id(local_id)
        {
            if let Some(anon) = field.default {
                record!(self.tables.default_fields[def_id] <- anon.def_id.to_def_id());
            }
            self.safety(def_id.index, field.safety, |_, safety| safety);
            let mut_restriction = match field.mut_restriction.kind {
                hir::RestrictionKind::Unrestricted => ty::RestrictionKind::Unrestricted,
                hir::RestrictionKind::Restricted(path) => {
                    ty::RestrictionKind::Restricted(path.res, field.mut_restriction.span)
                }
            };
            record!(self.tables.mut_restriction[def_id] <- mut_restriction);
        }

        let definition_spans = if should_encode_span(def_kind) {
            Some(tcx.metadata_definition_spans(local_id).0)
        } else {
            None
        };
        if let Some(spans) = definition_spans {
            record!(self.tables.def_span[def_id] <- spans.span);
        }
        if should_encode_attrs(def_kind) {
            self.encode_attrs(local_id);
        }
        if should_encode_expn_that_defined(def_kind) {
            record!(self.tables.expn_that_defined[def_id] <- self.tcx.expn_that_defined(def_id));
        }
        if let Some(spans) = definition_spans {
            if let Some(ident_span) = spans.ident {
                record!(self.tables.def_ident_span[def_id] <- ident_span);
            }
        }
        if def_kind.has_codegen_attrs() {
            record!(self.tables.codegen_fn_attrs[def_id] <- self.tcx.codegen_fn_attrs(def_id));
        }
        if should_encode_visibility(def_kind) {
            let visibility = self.tcx.local_visibility(local_id);
            self.visibility(def_id.index, visibility, |encoder, visibility| {
                encoder.lazy(visibility.map_id(|mod_id| mod_id.to_local_def_id().local_def_index))
            });
        }
        if should_encode_stability(def_kind) {
            self.encode_stability(def_id);
            self.encode_const_stability(def_id);
            self.encode_default_body_stability(def_id);
            self.encode_deprecation(def_id);
        }
        if should_encode_variances(tcx, def_id, def_kind) {
            let v = self.tcx.variances_of(def_id);
            record_array!(self.tables.variances_of[def_id] <- v);
        }
        if should_encode_fn_sig(def_kind) {
            record!(self.tables.fn_sig[def_id] <- tcx.fn_sig(def_id));
        }
        if should_encode_generics(def_kind) {
            let g = tcx.generics_of(def_id);
            record!(self.tables.generics_of[def_id] <- g);
            record!(self.tables.explicit_clauses_of[def_id] <- self.tcx.explicit_clauses_of(def_id));
            let inferred_outlives = self.tcx.inferred_outlives_of(def_id);
            record_array!(self.tables.inferred_outlives_of[def_id] <- inferred_outlives);
        }
        if def_kind == DefKind::ConstParam
            && let Some(param) = tcx
                .generics_of(tcx.parent(def_id))
                .own_params
                .iter()
                .find(|param| param.def_id == def_id)
            && let ty::GenericParamDefKind::Const { has_default: true, .. } = param.kind
        {
            let default = tcx.const_param_default(def_id);
            record!(self.tables.const_param_default[def_id] <- default);
        }
        if tcx.is_conditionally_const(def_id) {
            record!(self.tables.const_conditions[def_id] <- self.tcx.const_conditions(def_id));
        }
        if should_encode_type(tcx, local_id, def_kind) {
            record!(self.tables.type_of[def_id] <- self.tcx.type_of(def_id));
        }
        if should_encode_constness(def_kind) {
            let constness = self.tcx.constness(def_id);
            self.constness(def_id.index, constness, |_, constness| constness);
        }
        if let DefKind::Fn | DefKind::AssocFn = def_kind {
            let asyncness = tcx.asyncness(def_id);
            self.asyncness(def_id.index, asyncness, |_, asyncness| asyncness);
            record_array!(self.tables.fn_arg_idents[def_id] <- tcx.fn_arg_idents(def_id));
        }
        if let Some(name) = tcx.intrinsic(def_id) {
            self.intrinsic(def_id.index, name, |encoder, name| Some(encoder.lazy(name)));
        }
        if let DefKind::TyParam | DefKind::Trait = def_kind {
            let default = self.tcx.object_lifetime_default(def_id);
            record!(self.tables.object_lifetime_default[def_id] <- default);
        }
        if let DefKind::Trait = def_kind {
            record!(self.tables.trait_def[def_id] <- self.tcx.trait_def(def_id));
            record_array!(self.tables.explicit_super_clauses_of[def_id] <-
                    self.tcx.explicit_super_clauses_of(def_id).skip_binder());
            record_array!(self.tables.explicit_implied_clauses_of[def_id] <-
                    self.tcx.explicit_implied_clauses_of(def_id).skip_binder());
            let module_children = self
                .tcx
                .metadata_resolutions(())
                .0
                .module_children
                .get(&local_id)
                .map_or_default(|children| &children[..]);
            let module_children: Vec<_> = module_children
                .iter()
                .map(|child| child.res.def_id())
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect();
            self.module_children_non_reexports(
                def_id.index,
                module_children,
                |encoder, module_children| {
                    encoder.lazy_array(module_children.into_iter().map(|def_id| def_id.index))
                },
            );
            if self.tcx.is_const_trait(def_id) {
                record_array!(self.tables.explicit_implied_const_bounds[def_id]
                        <- self.tcx.explicit_implied_const_bounds(def_id).skip_binder());
            }
        }
        if let DefKind::TraitAlias = def_kind {
            record!(self.tables.trait_def[def_id] <- self.tcx.trait_def(def_id));
            record_array!(self.tables.explicit_super_clauses_of[def_id] <-
                    self.tcx.explicit_super_clauses_of(def_id).skip_binder());
            record_array!(self.tables.explicit_implied_clauses_of[def_id] <-
                    self.tcx.explicit_implied_clauses_of(def_id).skip_binder());
        }
        if let DefKind::Trait | DefKind::Impl { .. } = def_kind {
            let associated_item_def_ids = self.tcx.associated_item_def_ids(def_id);
            self.associated_item_or_field_def_ids(
                def_id.index,
                associated_item_def_ids,
                |encoder, associated_item_def_ids| {
                    encoder.lazy_array(associated_item_def_ids.iter().copied().map(|def_id| {
                        assert!(def_id.is_local());
                        def_id.index
                    }))
                },
            );
        }
        if def_kind == (DefKind::Impl { of_trait: true }) {
            let header = tcx.impl_trait_header(def_id);
            record!(self.tables.impl_trait_header[def_id] <- header);

            let impl_is_fully_generic_for_reflection =
                tcx.impl_is_fully_generic_for_reflection(def_id);
            self.impl_is_fully_generic_for_reflection(
                def_id.index,
                impl_is_fully_generic_for_reflection,
                |_, impl_is_fully_generic_for_reflection| impl_is_fully_generic_for_reflection,
            );

            let defaultness = tcx.defaultness(def_id);
            self.defaultness(def_id.index, defaultness, |_, defaultness| defaultness);

            let trait_ref = header.trait_ref.instantiate_identity().skip_norm_wip();
            let trait_def = tcx.trait_def(trait_ref.def_id);
            if let Ok(mut ancestors) = trait_def.ancestors(tcx, def_id)
                && let Some(specialization_graph::Node::Impl(parent)) = ancestors.nth(1)
            {
                self.impl_parent(def_id.index, parent, RawDefId::new);
            }

            if tcx.is_lang_item(trait_ref.def_id, LangItem::CoerceUnsized) {
                let coerce_unsized_info = tcx.coerce_unsized_info(def_id).unwrap();
                record!(self.tables.coerce_unsized_info[def_id] <- coerce_unsized_info);
            }
        }
        if let DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy = def_kind {
            self.encode_info_for_assoc_item(def_id);
        }
        if let DefKind::Closure | DefKind::SyntheticCoroutineBody = def_kind
            && let Some(coroutine_kind) = self.tcx.coroutine_kind(def_id)
        {
            self.coroutine_kind(def_id.index, coroutine_kind, |_, coroutine_kind| coroutine_kind);
        }
        if def_kind == DefKind::Closure && tcx.type_of(def_id).skip_binder().is_coroutine_closure()
        {
            let coroutine_for_closure = self.tcx.coroutine_for_closure(def_id);
            self.coroutine_for_closure(def_id.index, coroutine_for_closure, RawDefId::new);

            // If this async closure has a by-move body, record it too.
            if tcx.needs_coroutine_by_move_body_def_id(coroutine_for_closure) {
                let by_move_body = self.tcx.coroutine_by_move_body_def_id(coroutine_for_closure);
                self.coroutine_by_move_body_def_id(
                    coroutine_for_closure.index,
                    by_move_body,
                    RawDefId::new,
                );
            }
        }
        if let DefKind::Static { .. } = def_kind {
            if !self.tcx.is_foreign_item(def_id) {
                let data = self.tcx.eval_static_initializer(def_id).unwrap();
                record!(self.tables.eval_static_initializer[def_id] <- data);
            }
        }
        if let DefKind::Enum | DefKind::Struct | DefKind::Union = def_kind {
            self.encode_info_for_adt(local_id);
        }
        if def_kind == DefKind::Variant {
            self.encode_info_for_variant(local_id);
        }
        if let DefKind::Mod = def_kind {
            self.encode_info_for_mod(local_id);
            let mod_id = LocalModId::new_unchecked(local_id);
            if let Some(res_map) = tcx.metadata_resolutions(()).0.doc_link_resolutions.get(&mod_id)
            {
                self.doc_link_resolutions(
                    local_id.local_def_index,
                    DocLinkResolutionsRecord::new(res_map),
                    |encoder, record| encoder.lazy_doc_link_resolutions(&record.encoded),
                );
            }
            if let Some(traits) =
                tcx.metadata_resolutions(()).0.doc_link_traits_in_scope.get(&mod_id)
            {
                record_array!(self.tables.doc_link_traits_in_scope[def_id] <- traits);
            }
        }
        if let DefKind::Macro(_) = def_kind {
            self.encode_info_for_macro(local_id);
        }
        if let DefKind::TyAlias = def_kind {
            let is_checked = self.tcx.type_alias_is_checked(def_id);
            self.type_alias_is_checked(def_id.index, is_checked, |_, is_checked| is_checked);
            if is_checked {
                record!(self.tables.args_known_to_outlive_alias_params[def_id] <- tcx.args_known_to_outlive_alias_params(def_id));
            }
        }
        if let DefKind::OpaqueTy = def_kind {
            self.encode_explicit_item_bounds(def_id);
            self.encode_explicit_item_self_bounds(def_id);
            record!(self.tables.opaque_ty_origin[def_id] <- self.tcx.opaque_ty_origin(def_id));
            self.encode_precise_capturing_args(def_id);
            if tcx.is_conditionally_const(def_id) {
                record_array!(self.tables.explicit_implied_const_bounds[def_id]
                        <- tcx.explicit_implied_const_bounds(def_id).skip_binder());
            }
            record!(self.tables.args_known_to_outlive_alias_params[def_id] <- tcx.args_known_to_outlive_alias_params(def_id));
        }
        if let DefKind::AssocTy = def_kind {
            let assoc_item = tcx.associated_item(def_id);
            match assoc_item.container {
                ty::AssocContainer::Trait => {
                    record!(self.tables.args_known_to_outlive_alias_params[def_id] <- tcx.args_known_to_outlive_alias_params(def_id));
                }
                ty::AssocContainer::InherentImpl => {
                    record!(self.tables.args_known_to_outlive_alias_params[def_id] <- tcx.args_known_to_outlive_alias_params(def_id));
                }
                ty::AssocContainer::TraitImpl(_) => {}
            }
        }
        if let DefKind::AnonConst = def_kind {
            record!(self.tables.anon_const_kind[def_id] <- self.tcx.anon_const_kind(def_id));
        }
        if should_encode_const_of_item(self.tcx, def_id, def_kind) {
            record!(self.tables.const_of_item[def_id] <- self.tcx.const_of_item(def_id));
        }
        if tcx.impl_method_has_trait_impl_trait_tys(def_id)
            && let Ok(table) = self.tcx.collect_return_position_impl_trait_in_trait_tys(def_id)
        {
            let table = CanonicalDefIdMap::new(tcx, table);
            self.collect_return_position_impl_trait_in_trait_tys(
                def_id.index,
                table,
                |encoder, table| encoder.lazy_def_id_map(&table.encoded),
            );
        }
        if let DefKind::Impl { .. } | DefKind::Trait = def_kind {
            let table = tcx.associated_types_for_impl_traits_in_trait_or_impl(def_id);
            let table = CanonicalDefIdMap::new(tcx, table);
            self.associated_types_for_impl_traits_in_trait_or_impl(
                def_id.index,
                table,
                |encoder, table| encoder.lazy_def_id_map(&table.encoded),
            );
        }
        self.encode_mir(local_id);

        if let Some(impls) = tcx.crate_inherent_impls(()).0.inherent_impls.get(&local_id) {
            let impls: Vec<_> = impls
                .iter()
                .copied()
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect();
            if !impls.is_empty() {
                self.inherent_impls(local_id.local_def_index, impls, |encoder, impls| {
                    encoder.lazy_array(impls.into_iter().map(|def_id| {
                        assert!(def_id.is_local());
                        def_id.index
                    }))
                });
            }
        }
    }

    fn encode_externally_implementable_items(&mut self) -> LazyArray<EiiMapEncodedKeyValue> {
        let externally_implementable_items = self.tcx.externally_implementable_items(LOCAL_CRATE);
        let externally_implementable_items = if self.is_proc_macro() {
            Vec::new()
        } else {
            let mut selected_items = Vec::new();
            for (foreign_item, (decl, impls)) in externally_implementable_items.iter() {
                if !self.opaque.contains_def_id(*foreign_item) {
                    continue;
                }
                let impls = impls
                    .iter()
                    .filter(|(impl_did, _)| self.opaque.contains_def_id(**impl_did))
                    .map(|(impl_did, index)| (*impl_did, *index))
                    .collect();
                selected_items.push((*foreign_item, (decl.clone(), impls)));
            }
            selected_items
        };
        self.externally_implementable_items(
            externally_implementable_items,
            |encoder, externally_implementable_items| {
                encoder.lazy_array(externally_implementable_items)
            },
        )
    }

    #[instrument(level = "trace", skip(self))]
    fn encode_info_for_adt(&mut self, local_def_id: LocalDefId) {
        let def_id = local_def_id.to_def_id();
        let tcx = self.tcx;
        let adt_def = tcx.adt_def(def_id);
        record!(self.tables.repr_options[def_id] <- adt_def.repr());

        let params_in_repr = self.tcx.params_in_repr(def_id);
        record!(self.tables.params_in_repr[def_id] <- params_in_repr);

        if adt_def.is_enum() {
            let module_children = tcx
                .metadata_resolutions(())
                .0
                .module_children
                .get(&local_def_id)
                .map_or_default(|children| &children[..]);
            let module_children: Vec<_> = module_children
                .iter()
                .map(|child| child.res.def_id())
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect();
            self.module_children_non_reexports(
                def_id.index,
                module_children,
                |encoder, module_children| {
                    encoder.lazy_array(module_children.into_iter().map(|def_id| def_id.index))
                },
            );
        } else {
            // For non-enum, there is only one variant, and its def_id is the adt's.
            debug_assert_eq!(adt_def.variants().len(), 1);
            debug_assert_eq!(adt_def.non_enum_variant().def_id, def_id);
            // Therefore, the loop over variants will encode its fields as the adt's children.
        }

        if !adt_def.is_enum() {
            let idx = FIRST_VARIANT;
            let variant = adt_def.non_enum_variant();
            let data = (idx, variant.discr, variant.ctor, variant.is_field_list_non_exhaustive());
            self.variant_data(def_id.index, data, |encoder, data| {
                encoder.lazy(VariantData {
                    idx: data.0,
                    discr: data.1,
                    ctor: data.2.map(|(kind, def_id)| (kind, def_id.index)),
                    is_non_exhaustive: data.3,
                })
            });

            let fields: Vec<_> = variant
                .fields
                .iter()
                .map(|field| field.did)
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect();
            self.associated_item_or_field_def_ids(
                variant.def_id.index,
                fields,
                |encoder, fields| {
                    encoder.lazy_array(fields.into_iter().map(|def_id| {
                        assert!(def_id.is_local());
                        def_id.index
                    }))
                },
            );

            if let Some((CtorKind::Fn, ctor_def_id)) = variant.ctor {
                let fn_sig = tcx.fn_sig(ctor_def_id);
                record!(self.tables.fn_sig[def_id] <- fn_sig);
            }
        }

        if let Some(destructor) = tcx.adt_destructor(local_def_id) {
            record!(self.tables.adt_destructor[def_id] <- destructor);
        }

        if let Some(destructor) = tcx.adt_async_destructor(local_def_id) {
            record!(self.tables.adt_async_destructor[def_id] <- destructor);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_info_for_variant(&mut self, local_def_id: LocalDefId) {
        let def_id = local_def_id.to_def_id();
        let tcx = self.tcx;
        let adt_def = tcx.adt_def(tcx.parent(def_id));
        let idx = adt_def.variant_index_with_id(def_id);
        let variant = &adt_def.variants()[idx];
        let data = (idx, variant.discr, variant.ctor, variant.is_field_list_non_exhaustive());
        self.variant_data(def_id.index, data, |encoder, data| {
            encoder.lazy(VariantData {
                idx: data.0,
                discr: data.1,
                ctor: data.2.map(|(kind, def_id)| (kind, def_id.index)),
                is_non_exhaustive: data.3,
            })
        });

        let fields: Vec<_> = variant
            .fields
            .iter()
            .map(|field| field.did)
            .filter(|&def_id| self.opaque.contains_def_id(def_id))
            .collect();
        self.associated_item_or_field_def_ids(def_id.index, fields, |encoder, fields| {
            encoder.lazy_array(fields.into_iter().map(|def_id| {
                assert!(def_id.is_local());
                def_id.index
            }))
        });

        if let Some((CtorKind::Fn, ctor_def_id)) = variant.ctor {
            let fn_sig = tcx.fn_sig(ctor_def_id);
            record!(self.tables.fn_sig[def_id] <- fn_sig);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_info_for_mod(&mut self, local_def_id: LocalDefId) {
        let tcx = self.tcx;
        let def_id = local_def_id.to_def_id();

        // If we are encoding a proc-macro crates, `encode_info_for_mod` will
        // only ever get called for the crate root. We still want to encode
        // the crate root for consistency with other crates (some of the resolver
        // code uses it). However, we skip encoding anything relating to child
        // items - we encode information about proc-macros later on.
        if self.is_proc_macro() {
            // Encode this here because we don't do it in encode_def_ids.
            record!(self.tables.expn_that_defined[def_id] <- tcx.expn_that_defined(local_def_id));
        } else {
            let module_children = tcx
                .metadata_resolutions(())
                .0
                .module_children
                .get(&local_def_id)
                .map_or_default(|children| &children[..]);

            let mut non_reexports: Vec<_> = module_children
                .iter()
                .filter(|child| {
                    child.reexport_chain.is_empty() && self.opaque.contains_module_child(child)
                })
                .map(|child| child.res.def_id())
                .collect();
            if self.opaque.is_rdr() {
                non_reexports.sort_unstable_by_key(|def_id| tcx.def_path_hash(*def_id));
            }
            self.module_children_non_reexports(
                def_id.index,
                non_reexports,
                |encoder, non_reexports| {
                    encoder.lazy_array(non_reexports.into_iter().map(|def_id| def_id.index))
                },
            );

            record_array!(self.tables.module_children_reexports[def_id] <-
            module_children.iter()
                .filter(|child| {
                    !child.reexport_chain.is_empty()
                        && self.opaque.contains_module_child(child)
                }));

            let ambig_module_children = tcx
                .metadata_resolutions(())
                .0
                .ambig_module_children
                .get(&local_def_id)
                .map_or_default(|v| &v[..]);
            record_array!(self.tables.ambig_module_children[def_id] <-
            ambig_module_children.iter().filter(|child| {
                self.opaque.contains_module_child(&child.main)
                    && self.opaque.contains_module_child(&child.second)
            }));
        }
    }

    fn encode_explicit_item_bounds(&mut self, def_id: DefId) {
        debug!("EncodeContext::encode_explicit_item_bounds({:?})", def_id);
        let bounds = self.tcx.explicit_item_bounds(def_id).skip_binder();
        record_array!(self.tables.explicit_item_bounds[def_id] <- bounds);
    }

    fn encode_explicit_item_self_bounds(&mut self, def_id: DefId) {
        debug!("EncodeContext::encode_explicit_item_self_bounds({:?})", def_id);
        let bounds = self.tcx.explicit_item_self_bounds(def_id).skip_binder();
        record_array!(self.tables.explicit_item_self_bounds[def_id] <- bounds);
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_info_for_assoc_item(&mut self, def_id: DefId) {
        let tcx = self.tcx;
        let item = tcx.associated_item(def_id);

        if matches!(item.container, AssocContainer::Trait | AssocContainer::TraitImpl(_)) {
            let defaultness = item.defaultness(tcx);
            self.defaultness(def_id.index, defaultness, |_, defaultness| defaultness);
        }

        record!(self.tables.assoc_container[def_id] <- item.container);

        if let AssocContainer::Trait = item.container
            && item.is_type()
        {
            self.encode_explicit_item_bounds(def_id);
            self.encode_explicit_item_self_bounds(def_id);
            if tcx.is_conditionally_const(def_id) {
                record_array!(self.tables.explicit_implied_const_bounds[def_id]
                    <- self.tcx.explicit_implied_const_bounds(def_id).skip_binder());
            }
        }
        if let ty::AssocKind::Type { data: ty::AssocTypeData::Rpitit(rpitit_info) } = item.kind {
            self.opt_rpitit_info(def_id.index, rpitit_info, |encoder, rpitit_info| {
                Some(encoder.lazy(rpitit_info))
            });
            if matches!(rpitit_info, ty::ImplTraitInTraitData::Trait { .. }) {
                record_array!(
                    self.tables.assumed_wf_types_for_rpitit[def_id]
                        <- self.tcx.assumed_wf_types_for_rpitit(def_id)
                );
                self.encode_precise_capturing_args(def_id);
            }
        }
    }

    fn encode_precise_capturing_args(&mut self, def_id: DefId) {
        let Some(precise_capturing_args) = self.tcx.rendered_precise_capturing_args(def_id) else {
            return;
        };

        record_array!(self.tables.rendered_precise_capturing_args[def_id] <- precise_capturing_args);
    }

    fn encode_mir(&mut self, def_id: LocalDefId) {
        if self.is_proc_macro() || !self.tcx.mir_keys(()).contains(&def_id) {
            return;
        }

        let tcx = self.tcx;
        let reachable_set = tcx.reachable_set(());
        let (encode_const, encode_opt) = should_encode_mir(tcx, reachable_set, def_id);
        if encode_const || encode_opt {
            debug_assert!(encode_const || encode_opt);
            debug!("EntryBuilder::encode_mir({:?})", def_id);
            if encode_opt {
                record!(self.tables.optimized_mir[def_id.to_def_id()] <- tcx.optimized_mir(def_id));
                let cross_crate_inlinable = self.tcx.cross_crate_inlinable(def_id);
                self.cross_crate_inlinable(
                    def_id.to_def_id().index,
                    cross_crate_inlinable,
                    |_, cross_crate_inlinable| cross_crate_inlinable,
                );
                record!(self.tables.closure_saved_names_of_captured_variables[def_id.to_def_id()]
                    <- tcx.closure_saved_names_of_captured_variables(def_id));
            }
            let mut is_trivial = false;
            if encode_const {
                if let Some((val, ty)) = tcx.trivial_const(def_id) {
                    is_trivial = true;
                    record!(self.tables.trivial_const[def_id.to_def_id()] <- (val, ty));
                } else {
                    is_trivial = false;
                    record!(self.tables.mir_for_ctfe[def_id.to_def_id()] <- tcx.mir_for_ctfe(def_id));
                }

                // FIXME(generic_const_exprs): this feels wrong to have in `encode_mir`
                let abstract_const = tcx.thir_abstract_const(def_id);
                if let Ok(Some(abstract_const)) = abstract_const {
                    record!(self.tables.thir_abstract_const[def_id.to_def_id()] <- abstract_const);
                }

                if should_encode_const(tcx.def_kind(def_id)) {
                    let qualifs = tcx.mir_const_qualif(def_id);
                    record!(self.tables.mir_const_qualif[def_id.to_def_id()] <- qualifs);
                    let body = tcx.hir_maybe_body_owned_by(def_id);
                    if let Some(body) = body {
                        let const_data = rendered_const(self.tcx, &body, def_id);
                        record!(self.tables.rendered_const[def_id.to_def_id()] <- const_data);
                    }
                }
            }
            if !is_trivial {
                record!(self.tables.promoted_mir[def_id.to_def_id()] <- tcx.promoted_mir(def_id));
            }

            if self.tcx.is_coroutine(def_id.to_def_id())
                && let Some(witnesses) = tcx.mir_coroutine_witnesses(def_id)
            {
                record!(self.tables.mir_coroutine_witnesses[def_id.to_def_id()] <- witnesses);
            }
        }

        // Encode all the deduced parameter attributes for everything that has MIR, even for items
        // that can't be inlined. But don't if we aren't optimizing in non-incremental mode, to
        // save the query traffic.
        if tcx.sess.opts.output_types.should_codegen()
            && tcx.sess.opts.optimize != OptLevel::No
            && tcx.sess.opts.incremental.is_none()
        {
            if let DefKind::AssocFn | DefKind::Fn = tcx.def_kind(def_id) {
                record_array!(self.tables.deduced_param_attrs[def_id.to_def_id()] <-
                    self.tcx.deduced_param_attrs(def_id.to_def_id()));
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_stability(&mut self, def_id: DefId) {
        // The query lookup can take a measurable amount of time in crates with many items. Check if
        // the stability attributes are even enabled before using their queries.
        if self.feat.staged_api() || self.tcx.sess.opts.unstable_opts.force_unstable_if_unmarked {
            if let Some(stab) = self.tcx.lookup_stability(def_id) {
                record!(self.tables.lookup_stability[def_id] <- stab)
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_const_stability(&mut self, def_id: DefId) {
        // The query lookup can take a measurable amount of time in crates with many items. Check if
        // the stability attributes are even enabled before using their queries.
        if self.feat.staged_api() || self.tcx.sess.opts.unstable_opts.force_unstable_if_unmarked {
            if let Some(stab) = self.tcx.lookup_const_stability(def_id) {
                record!(self.tables.lookup_const_stability[def_id] <- stab)
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_default_body_stability(&mut self, def_id: DefId) {
        // The query lookup can take a measurable amount of time in crates with many items. Check if
        // the stability attributes are even enabled before using their queries.
        if self.feat.staged_api() || self.tcx.sess.opts.unstable_opts.force_unstable_if_unmarked {
            if let Some(stab) = self.tcx.lookup_default_body_stability(def_id) {
                record!(self.tables.lookup_default_body_stability[def_id] <- stab)
            }
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_deprecation(&mut self, def_id: DefId) {
        if let Some(depr) = self.tcx.lookup_deprecation(def_id) {
            record!(self.tables.lookup_deprecation_entry[def_id] <- depr);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_info_for_macro(&mut self, def_id: LocalDefId) {
        let tcx = self.tcx;

        let (_, macro_def, _) = tcx.hir_expect_item(def_id).expect_macro();
        self.is_macro_rules(def_id.local_def_index, macro_def.macro_rules, |_, is_macro_rules| {
            is_macro_rules
        });
        record!(self.tables.macro_definition[def_id.to_def_id()] <- &*macro_def.body);
    }

    fn encode_native_libraries(&mut self) -> LazyArray<NativeLib> {
        let used_libraries: &[NativeLib] =
            if self.is_proc_macro() { &[] } else { self.tcx.native_libraries(LOCAL_CRATE) };
        self.native_libraries(used_libraries, |encoder, used_libraries| {
            encoder.lazy_array(used_libraries.iter())
        })
    }

    fn encode_foreign_modules(&mut self) -> LazyArray<ForeignModule> {
        let foreign_modules: Vec<_> = if self.is_proc_macro() {
            Vec::new()
        } else {
            let mut selected_modules = Vec::new();
            for (_, module) in self.tcx.foreign_modules(LOCAL_CRATE) {
                if !self.opaque.contains_def_id(module.def_id) {
                    continue;
                }
                let mut module = module.clone();
                module.foreign_items.retain(|&def_id| self.opaque.contains_def_id(def_id));
                selected_modules.push(module);
            }
            selected_modules
        };
        self.foreign_modules(foreign_modules, |encoder, foreign_modules| {
            encoder.lazy_array(foreign_modules)
        })
    }

    fn encode_hygiene(
        &mut self,
    ) -> (SyntaxContextTable, ExpnDataTable, ExpnHashTable, HygieneDelta) {
        let mut syntax_contexts: TableBuilder<_, _> = Default::default();
        let mut expn_data_table: TableBuilder<_, _> = Default::default();
        let mut expn_hash_table: TableBuilder<_, _> = Default::default();
        // Hygiene is semantically unordered even though artifact indices determine its wire layout.
        let mut semantic_syntax_contexts = UnordBag::new();
        let mut semantic_expn_data = UnordBag::new();
        let mut semantic_expn_hashes = UnordBag::new();

        let hygiene_ctxt = Arc::clone(&self.hygiene_ctxt);
        let encoded_hygiene = hygiene_ctxt.encode_pending(
            &mut (
                &mut *self,
                &mut syntax_contexts,
                &mut expn_data_table,
                &mut expn_hash_table,
                &mut semantic_syntax_contexts,
                &mut semantic_expn_data,
                &mut semantic_expn_hashes,
            ),
            |(this, syntax_contexts, _, _, semantic_syntax_contexts, _, _), context| {
                semantic_syntax_contexts.push((context.identity(), *context.data()));
                let record = PersistedRecord::CrateRoot(PersistedCrateRootField::syntax_contexts);
                let ctxt_data =
                    this.with_wire_record(record, |encoder| encoder.lazy(context.data()));
                syntax_contexts.set_some(context.index(), ctxt_data);
            },
            |(
                this,
                _,
                expn_data_table,
                expn_hash_table,
                _,
                semantic_expn_data,
                semantic_expn_hashes,
            ),
             expansion| {
                if expansion.id().as_local().is_some() {
                    let encoded_index = expansion.index();
                    semantic_expn_data
                        .push((expansion.identity(), MetadataExpnData(expansion.data().clone())));
                    let record = PersistedRecord::CrateRoot(PersistedCrateRootField::expn_data);
                    let expn_data =
                        this.with_wire_record(record, |encoder| encoder.lazy(expansion.data()));
                    expn_data_table.set_some(encoded_index, expn_data);
                    if this.visits(PersistedRecord::CrateRoot(PersistedCrateRootField::expn_hashes))
                    {
                        semantic_expn_hashes.push((expansion.identity(), expansion.hash()));
                        let record =
                            PersistedRecord::CrateRoot(PersistedCrateRootField::expn_hashes);
                        let hash =
                            this.with_wire_record(record, |encoder| encoder.lazy(expansion.hash()));
                        expn_hash_table.set_some(encoded_index, hash);
                    }
                }
            },
        );

        let syntax_contexts =
            self.syntax_contexts(semantic_syntax_contexts, |encoder, _semantic| {
                syntax_contexts.encode(encoder.position(), |bytes| encoder.emit_raw_bytes(bytes))
            });
        let expn_data = self.expn_data(semantic_expn_data, |encoder, _semantic| {
            expn_data_table.encode(encoder.position(), |bytes| encoder.emit_raw_bytes(bytes))
        });
        let expn_hashes = if self
            .visits(PersistedRecord::CrateRoot(PersistedCrateRootField::expn_hashes))
        {
            self.expn_hashes(semantic_expn_hashes, |encoder, _semantic| {
                expn_hash_table.encode(encoder.position(), |bytes| encoder.emit_raw_bytes(bytes))
            })
        } else {
            LazyTable::default()
        };
        (syntax_contexts, expn_data, expn_hashes, encoded_hygiene)
    }

    fn encode_proc_macros(&mut self) -> Option<ProcMacroData> {
        let is_proc_macro = self.tcx.crate_types().contains(&CrateType::ProcMacro);
        if is_proc_macro {
            let tcx = self.tcx;
            let proc_macro_decls_static = tcx.proc_macro_decls_static(()).unwrap().local_def_index;
            let stability = tcx.lookup_stability(CRATE_DEF_ID);
            for (i, span) in self.tcx.sess.proc_macro_quoted_spans() {
                self.proc_macro_quoted_spans(i, span, |encoder, span| encoder.lazy(span));
            }

            self.def_kind(LOCAL_CRATE.as_def_id().index, DefKind::Mod, |_, def_kind| def_kind);
            record!(self.tables.def_span[LOCAL_CRATE.as_def_id()] <- tcx.def_span(LOCAL_CRATE.as_def_id()));
            self.encode_attrs(LOCAL_CRATE.as_def_id().expect_local());
            let visibility = tcx.local_visibility(CRATE_DEF_ID);
            self.visibility(LOCAL_CRATE.as_def_id().index, visibility, |encoder, visibility| {
                encoder.lazy(visibility.map_id(|mod_id| mod_id.to_local_def_id().local_def_index))
            });
            if let Some(stability) = stability {
                record!(self.tables.lookup_stability[LOCAL_CRATE.as_def_id()] <- stability);
            }
            self.encode_deprecation(LOCAL_CRATE.as_def_id());
            if let Some(res_map) =
                tcx.metadata_resolutions(()).0.doc_link_resolutions.get(&CRATE_MOD_ID)
            {
                self.doc_link_resolutions(
                    LOCAL_CRATE.as_def_id().index,
                    DocLinkResolutionsRecord::new(res_map),
                    |encoder, record| encoder.lazy_doc_link_resolutions(&record.encoded),
                );
            }
            if let Some(traits) =
                tcx.metadata_resolutions(()).0.doc_link_traits_in_scope.get(&CRATE_MOD_ID)
            {
                record_array!(self.tables.doc_link_traits_in_scope[LOCAL_CRATE.as_def_id()] <- traits);
            }

            let mut macros = vec![];

            // Normally, this information is encoded when we walk the items
            // defined in this crate. However, we skip doing that for proc-macro crates,
            // so we manually encode just the information that we need
            for &proc_macro in &tcx.metadata_resolutions(()).0.proc_macros {
                let id = proc_macro;
                let proc_macro = tcx.local_def_id_to_hir_id(proc_macro);
                let mut name = tcx.hir_name(proc_macro);
                let span = tcx.hir_span(proc_macro);
                // Proc-macros may have attributes like `#[allow_internal_unstable]`,
                // so downstream crates need access to them.
                let attrs = tcx.metadata_attrs(id).0;
                let (macro_kind, kind) = if find_attr!(attrs, ProcMacro) {
                    (MacroKind::Bang, ProcMacroKind::Bang { name: name.as_str().to_owned() })
                } else if find_attr!(attrs, ProcMacroAttribute) {
                    (MacroKind::Attr, ProcMacroKind::Attr { name: name.as_str().to_owned() })
                } else if let Some((trait_name, helper_attrs)) = find_attr!(attrs,
                    ProcMacroDerive { trait_name, helper_attrs } => (trait_name, helper_attrs))
                {
                    name = *trait_name;
                    (
                        MacroKind::Derive,
                        ProcMacroKind::CustomDerive {
                            trait_name: name.as_str().to_owned(),
                            attributes: helper_attrs
                                .iter()
                                .map(|attr| attr.as_str().to_owned())
                                .collect(),
                        },
                    )
                } else {
                    bug!("Unknown proc-macro type for item {:?}", id);
                };

                macros.push((id.local_def_index, self.lazy(kind)));

                let mut def_key = self.tcx.hir_def_key(id);
                def_key.disambiguated_data.data = DefPathData::MacroNs(name);

                let def_id = id.to_def_id();
                self.def_kind(def_id.index, DefKind::Macro(macro_kind.into()), |_, def_kind| {
                    def_kind
                });
                self.encode_attrs(id);
                self.def_keys(
                    def_id.index,
                    DefKeyRecord { hash: tcx.def_path_hash(def_id), key: def_key },
                    |encoder, record| encoder.lazy(record.key),
                );
                record!(self.tables.def_ident_span[def_id] <- span);
                record!(self.tables.def_span[def_id] <- span);
                self.visibility(
                    def_id.index,
                    ty::Visibility::<DefId>::Public,
                    |encoder, visibility| encoder.lazy(visibility.map_id(|def_id| def_id.index)),
                );
                if let Some(stability) = stability {
                    record!(self.tables.lookup_stability[def_id] <- stability);
                }
            }

            let macros = self.lazy_array(macros);

            Some(ProcMacroData { proc_macro_decls_static, stability, macros })
        } else {
            None
        }
    }

    fn encode_debugger_visualizers(&mut self) -> LazyArray<DebuggerVisualizerFile> {
        let debugger_visualizers: Vec<_> = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .debugger_visualizers(LOCAL_CRATE)
                .iter()
                // Erase the path since it may contain privacy sensitive data
                // that we don't want to end up in crate metadata.
                // The path is only needed for the local crate because of
                // `--emit dep-info`.
                .map(DebuggerVisualizerFile::path_erased)
                .collect()
        };
        self.debugger_visualizers(debugger_visualizers, |encoder, debugger_visualizers| {
            encoder.lazy_array(debugger_visualizers)
        })
    }

    fn encode_crate_deps(&mut self) -> LazyArray<CrateDep> {
        let deps = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .crates(())
                .iter()
                .map(|&cnum| {
                    let dep = CrateDep {
                        name: self.tcx.crate_name(cnum),
                        hash: self.tcx.crate_hash(cnum),
                        host_hash: self.tcx.crate_host_hash(cnum),
                        kind: self.tcx.crate_dep_kind(cnum),
                        extra_filename: self.tcx.extra_filename(cnum).clone(),
                        is_private: self.tcx.is_private_dep(cnum),
                    };
                    (cnum, dep)
                })
                .collect::<Vec<_>>()
        };

        {
            // Sanity-check the crate numbers
            let mut expected_cnum = 1;
            for &(n, _) in &deps {
                assert_eq!(n, CrateNum::new(expected_cnum));
                expected_cnum += 1;
            }
        }

        // We're just going to write a list of crate 'name-hash-version's, with
        // the assumption that they are numbered 1 to n.
        // FIXME (#2166): This is not nearly enough to support correct versioning
        // but is enough to get transitive crate dependencies working.
        self.crate_deps(deps, |encoder, deps| encoder.lazy_array(deps.iter().map(|(_, dep)| dep)))
    }

    fn encode_target_modifiers(&mut self) -> LazyArray<TargetModifier> {
        let target_modifiers = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx.sess.opts.gather_target_modifiers()
        };
        self.target_modifiers(target_modifiers, |encoder, target_modifiers| {
            encoder.lazy_array(target_modifiers)
        })
    }

    fn encode_enabled_denied_partial_mitigations(&mut self) -> LazyArray<DeniedPartialMitigation> {
        let mitigations = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx.sess.gather_enabled_denied_partial_mitigations()
        };
        self.denied_partial_mitigations(mitigations, |encoder, mitigations| {
            encoder.lazy_array(mitigations)
        })
    }

    fn encode_lib_features(&mut self) -> LazyArray<(Symbol, FeatureStability)> {
        let lib_features = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx.lib_features(LOCAL_CRATE).to_sorted_vec()
        };
        self.lib_features(lib_features, |encoder, lib_features| encoder.lazy_array(lib_features))
    }

    fn encode_stability_implications(&mut self) -> LazyArray<(Symbol, Symbol)> {
        let implications = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .stability_implications(LOCAL_CRATE)
                .to_sorted_stable_ord()
                .into_iter()
                .map(|(cause, implication)| (*cause, *implication))
                .collect()
        };
        self.stability_implications(implications, |encoder, implications| {
            encoder.lazy_array(implications)
        })
    }

    fn encode_canonical_symbols(&mut self) -> LazyArray<(Symbol, DefIndex)> {
        let canonical_symbols: Vec<_> = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .canonical_symbols(LOCAL_CRATE)
                .iter()
                .filter(|symbol| self.opaque.contains_def_id(symbol.def_id))
                .map(|symbol| (symbol.symbol, symbol.def_id))
                .collect()
        };
        self.canonical_symbols(canonical_symbols, |encoder, canonical_symbols| {
            encoder.lazy_array(
                canonical_symbols.into_iter().map(|(symbol, def_id)| (symbol, def_id.index)),
            )
        })
    }

    fn encode_diagnostic_items(&mut self) -> LazyArray<(Symbol, DefIndex)> {
        let diagnostic_items: Vec<_> = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .diagnostic_items(LOCAL_CRATE)
                .name_to_id
                .iter()
                .filter(|&(_, &def_id)| self.opaque.contains_def_id(def_id))
                .map(|(&name, &def_id)| (name, def_id))
                .collect()
        };
        self.diagnostic_items(diagnostic_items, |encoder, diagnostic_items| {
            encoder
                .lazy_array(diagnostic_items.into_iter().map(|(name, def_id)| (name, def_id.index)))
        })
    }

    fn encode_lang_items(&mut self) -> LazyArray<(DefIndex, LangItem)> {
        let lang_items: Vec<_> = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .lang_items()
                .iter()
                .filter_map(|(lang_item, def_id)| def_id.as_local().map(|id| (id, lang_item)))
                .filter(|(def_id, _)| self.opaque.contains_def_id(def_id.to_def_id()))
                .collect()
        };
        self.lang_items(lang_items, |encoder, lang_items| {
            encoder.lazy_array(
                lang_items.into_iter().map(|(def_id, item)| (def_id.local_def_index, item)),
            )
        })
    }

    fn encode_lang_items_missing(&mut self) -> LazyArray<LangItem> {
        let missing =
            if self.is_proc_macro() { Vec::new() } else { self.tcx.lang_items().missing.clone() };
        self.lang_items_missing(missing, |encoder, missing| encoder.lazy_array(missing))
    }

    fn encode_stripped_cfg_items(&mut self) -> LazyArray<StrippedCfgItem<DefIndex>> {
        let stripped_cfg_items: Vec<_> = self
            .tcx
            .stripped_cfg_items(LOCAL_CRATE)
            .iter()
            .filter(|item| {
                if !self.opaque.is_rdr() {
                    return true;
                }
                if item.visibility != StrippedCfgItemVisibility::Public
                    || !self.opaque.contains_def_id(item.parent_scope)
                {
                    return false;
                }

                let parent_scope = item.parent_scope.expect_local();
                let module_children = self
                    .tcx
                    .metadata_resolutions(())
                    .0
                    .module_children
                    .get(&parent_scope)
                    .map_or_default(|children| &children[..]);
                let has_selected_child = |namespace| {
                    module_children.iter().any(|child| {
                        child.ident.name == item.ident.name
                            && child.res.ns() == Some(namespace)
                            && child
                                .res
                                .opt_def_id()
                                .is_none_or(|def_id| self.opaque.contains_def_id(def_id))
                    })
                };
                let is_shadowed = [Namespace::TypeNS, Namespace::ValueNS, Namespace::MacroNS]
                    .into_iter()
                    .filter(|&namespace| item.namespaces.contains(namespace))
                    .all(has_selected_child);
                !is_shadowed
            })
            .cloned()
            .collect();
        self.stripped_cfg_items(stripped_cfg_items, |encoder, stripped_cfg_items| {
            encoder.lazy_array(
                stripped_cfg_items.into_iter().map(|item| item.map_scope_id(|def_id| def_id.index)),
            )
        })
    }

    fn encode_traits(&mut self) -> LazyArray<DefIndex> {
        let traits = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .traits(LOCAL_CRATE)
                .iter()
                .copied()
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect()
        };
        self.traits(traits, |encoder, traits| {
            encoder.lazy_array(traits.into_iter().map(|def_id| def_id.index))
        })
    }

    /// Encodes an index, mapping each trait to its (local) implementations.
    #[instrument(level = "debug", skip(self))]
    fn encode_impls(&mut self) -> LazyArray<TraitImpls> {
        empty_proc_macro!(self);
        let tcx = self.tcx;
        let mut trait_impls: FxIndexMap<DefId, Vec<(LocalDefId, Option<SimplifiedType>)>> =
            FxIndexMap::default();

        for id in tcx.hir_free_items() {
            let DefKind::Impl { of_trait } = tcx.def_kind(id.owner_id) else {
                continue;
            };
            let def_id = id.owner_id.to_def_id();

            if of_trait {
                let header = tcx.impl_trait_header(def_id);
                let trait_ref = header.trait_ref.instantiate_identity().skip_norm_wip();
                if !self.opaque.contains_trait_impl(id.owner_id.def_id, trait_ref) {
                    continue;
                }
                let simplified_self_ty = fast_reject::simplify_type(
                    self.tcx,
                    trait_ref.self_ty(),
                    TreatParams::InstantiateWithInfer,
                );
                trait_impls
                    .entry(trait_ref.def_id)
                    .or_default()
                    .push((id.owner_id.def_id, simplified_self_ty));
            }
        }

        let trait_impls: Vec<_> = trait_impls.into_iter().collect();
        self.impls(trait_impls, |encoder, trait_impls| {
            let trait_impls = trait_impls
                .into_iter()
                .map(|(trait_def_id, impls)| {
                    let trait_id = RawDefId::new(encoder, trait_def_id);
                    TraitImpls {
                        trait_id,
                        impls: encoder.lazy_array(
                            impls
                                .into_iter()
                                .map(|(def_id, simplified)| (def_id.local_def_index, simplified)),
                        ),
                    }
                })
                .collect::<Vec<_>>();
            encoder.lazy_array(&trait_impls)
        })
    }

    #[instrument(level = "debug", skip(self))]
    fn encode_incoherent_impls(&mut self) -> LazyArray<IncoherentImpls> {
        empty_proc_macro!(self);
        let tcx = self.tcx;

        let all_impls: Vec<_> = tcx
            .crate_inherent_impls(())
            .0
            .incoherent_impls
            .iter()
            .filter_map(|(&simplified, impls)| {
                let impls: Vec<_> = impls
                    .iter()
                    .copied()
                    .filter(|&def_id| self.opaque.contains_def_id(def_id.to_def_id()))
                    .collect();
                (!impls.is_empty()).then_some((simplified, impls))
            })
            .collect();
        self.incoherent_impls(all_impls, |encoder, all_impls| {
            let all_impls: Vec<_> = all_impls
                .into_iter()
                .map(|(simplified, impls)| IncoherentImpls {
                    self_ty: encoder.lazy(simplified),
                    impls: encoder
                        .lazy_array(impls.into_iter().map(|def_id| def_id.local_def_index)),
                })
                .collect();
            encoder.lazy_array(&all_impls)
        })
    }

    fn encode_exportable_items(&mut self) -> LazyArray<DefIndex> {
        let exportable_items = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .exportable_items(LOCAL_CRATE)
                .iter()
                .copied()
                .filter(|&def_id| self.opaque.contains_def_id(def_id))
                .collect()
        };
        self.exportable_items(exportable_items, |encoder, exportable_items| {
            encoder.lazy_array(exportable_items.into_iter().map(|def_id| def_id.index))
        })
    }

    fn encode_stable_order_of_exportable_impls(&mut self) -> LazyArray<(DefIndex, usize)> {
        let stable_order = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .stable_order_of_exportable_impls(LOCAL_CRATE)
                .iter()
                .filter(|&(&def_id, _)| self.opaque.contains_def_id(def_id))
                .map(|(&def_id, &index)| (def_id, index))
                .collect()
        };
        self.stable_order_of_exportable_impls(stable_order, |encoder, stable_order| {
            encoder
                .lazy_array(stable_order.into_iter().map(|(def_id, index)| (def_id.index, index)))
        })
    }

    fn encode_dylib_dependency_formats(&mut self) -> LazyArray<Option<LinkagePreference>> {
        let formats = if self.is_proc_macro() {
            Vec::new()
        } else {
            self.tcx
                .dependency_formats(())
                .get(&CrateType::Dylib)
                .map(|formats| formats.iter().skip(1).copied().collect())
                .unwrap_or_default()
        };
        self.dylib_dependency_formats(formats, |encoder, formats| {
            encoder.lazy_array(formats.into_iter().map(|linkage| match linkage {
                Linkage::NotLinked | Linkage::IncludedFromDylib => None,
                Linkage::Dynamic => Some(LinkagePreference::RequireDynamic),
                Linkage::Static => Some(LinkagePreference::RequireStatic),
            }))
        })
    }
}

/// Used to prefetch queries which will be needed later by metadata encoding.
/// Only a subset of the queries are actually prefetched to keep this code smaller.
fn prefetch_mir(tcx: TyCtxt<'_>) {
    if !tcx.sess.opts.output_types.should_codegen() {
        // We won't emit MIR, so don't prefetch it.
        return;
    }

    let reachable_set = tcx.reachable_set(());
    par_for_each_in(tcx.mir_keys(()), |&&def_id| {
        if tcx.is_trivial_const(def_id) {
            return;
        }
        let (encode_const, encode_opt) = should_encode_mir(tcx, reachable_set, def_id);

        if encode_const {
            tcx.ensure_done().mir_for_ctfe(def_id);
        }
        if encode_opt {
            tcx.ensure_done().optimized_mir(def_id);
        }
        if encode_opt || encode_const {
            tcx.ensure_done().promoted_mir(def_id);
        }
    })
}

// NOTE(eddyb) The following comment was preserved for posterity, even
// though it's no longer relevant as EBML (which uses nested & tagged
// "documents") was replaced with a scheme that can't go out of bounds.
//
// And here we run into yet another obscure archive bug: in which metadata
// loaded from archives may have trailing garbage bytes. Awhile back one of
// our tests was failing sporadically on the macOS 64-bit builders (both nopt
// and opt) by having ebml generate an out-of-bounds panic when looking at
// metadata.
//
// Upon investigation it turned out that the metadata file inside of an rlib
// (and ar archive) was being corrupted. Some compilations would generate a
// metadata file which would end in a few extra bytes, while other
// compilations would not have these extra bytes appended to the end. These
// extra bytes were interpreted by ebml as an extra tag, so they ended up
// being interpreted causing the out-of-bounds.
//
// The root cause of why these extra bytes were appearing was never
// discovered, and in the meantime the solution we're employing is to insert
// the length of the metadata to the start of the metadata. Later on this
// will allow us to slice the metadata to the precise length that we just
// generated regardless of trailing bytes that end up in it.

pub struct EncodedMetadata {
    // The declaration order matters because `full_metadata` should be dropped
    // before `_temp_dir`.
    full_metadata: Option<Mmap>,
    // This is an optional stub metadata containing only the crate header.
    // The header should be very small, so we load it directly into memory.
    stub_metadata: Option<Vec<u8>>,
    // The path containing the metadata, to record as work product.
    path: Option<Box<Path>>,
    // We need to carry MaybeTempDir to avoid deleting the temporary
    // directory while accessing the Mmap.
    _temp_dir: Option<MaybeTempDir>,
}

impl EncodedMetadata {
    #[inline]
    pub fn from_path(
        path: PathBuf,
        stub_path: Option<PathBuf>,
        temp_dir: Option<MaybeTempDir>,
    ) -> std::io::Result<Self> {
        let file = std::fs::File::open(&path)?;
        let file_metadata = file.metadata()?;
        if file_metadata.len() == 0 {
            return Ok(Self {
                full_metadata: None,
                stub_metadata: None,
                path: None,
                _temp_dir: None,
            });
        }
        let full_mmap = unsafe { Some(Mmap::map(file)?) };

        let stub =
            if let Some(stub_path) = stub_path { Some(std::fs::read(stub_path)?) } else { None };

        Ok(Self {
            full_metadata: full_mmap,
            stub_metadata: stub,
            path: Some(path.into()),
            _temp_dir: temp_dir,
        })
    }

    #[inline]
    pub fn full(&self) -> &[u8] {
        &self.full_metadata.as_deref().unwrap_or_default()
    }

    #[inline]
    pub fn stub_or_full(&self) -> &[u8] {
        self.stub_metadata.as_deref().unwrap_or(self.full())
    }

    #[inline]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl<S: Encoder> Encodable<S> for EncodedMetadata {
    fn encode(&self, s: &mut S) {
        self.stub_metadata.encode(s);

        let slice = self.full();
        slice.encode(s)
    }
}

impl<D: Decoder> Decodable<D> for EncodedMetadata {
    fn decode(d: &mut D) -> Self {
        let stub = <Option<Vec<u8>>>::decode(d);

        let len = d.read_usize();
        let full_metadata = if len > 0 {
            let mut mmap = MmapMut::map_anon(len).unwrap();
            mmap.copy_from_slice(d.read_raw_bytes(len));
            Some(mmap.make_read_only().unwrap())
        } else {
            None
        };

        Self { full_metadata, stub_metadata: stub, path: None, _temp_dir: None }
    }
}

#[instrument(level = "trace", skip(tcx))]
pub fn encode_metadata(tcx: TyCtxt<'_>, path: &Path, ref_path: Option<&Path>) {
    // Since encoding metadata is not in a query, and nothing is cached,
    // there's no need to do dep-graph tracking for any of it.
    tcx.dep_graph.assert_ignored();

    // Generate the metadata stub manually, as that is a small file compared to full metadata.
    if let Some(ref_path) = ref_path {
        let _prof_timer = tcx.prof.verbose_generic_activity("generate_crate_metadata_stub");
        let encoder = opaque::FileEncoder::new(ref_path)
            .unwrap_or_else(|err| tcx.dcx().emit_fatal(FailCreateFileEncoder { err }));
        with_encode_metadata_header(tcx, MetadataEncoding::CoarseStub { encoder }, |ecx| {
            let header: LazyValue<CrateHeader> = ecx.with_record(
                PersistedRecord::Artifact(PersistedArtifactRecord::stub_crate_header),
                |encoder| {
                    encoder.lazy(CrateHeader {
                        name: tcx.crate_name(LOCAL_CRATE),
                        triple: tcx.sess.opts.target_triple.clone(),
                        hash: tcx.crate_hash(LOCAL_CRATE),
                        is_proc_macro_crate: false,
                        is_stub: true,
                    })
                },
            );
            header.position.get()
        });
    }

    let dep_node = tcx.metadata_dep_node();

    // If the metadata dep-node is green, we can copy the saved work product.
    if tcx.dep_graph.is_fully_enabled()
        && let work_product_id = WorkProductId::from_cgu_name("metadata")
        && let Some(work_product) = tcx.dep_graph.previous_work_product(&work_product_id)
        && tcx.dep_graph.try_mark_green(tcx, &dep_node).is_some()
    {
        let saved_path = &work_product.saved_files["rmeta"];
        let incr_comp_session_dir = tcx.sess.incr_comp_session_dir();
        let source_file_in_incr_dir = &incr_comp_session_dir.join(saved_path);
        debug!("copying preexisting metadata from {source_file_in_incr_dir:?} to {path:?}");
        match rustc_fs_util::link_or_copy(&source_file_in_incr_dir, path) {
            Ok(_) => {}
            Err(err) => tcx.dcx().emit_fatal(FailCreateFileEncoder { err }),
        }
        return;
    }

    if tcx.sess.opts.jobs.frontend.is_some() {
        // Prefetch some queries used by metadata encoding.
        // This is not necessary for correctness, but is only done for performance reasons.
        // It can be removed if it turns out to cause trouble or be detrimental to performance.
        par_join(
            || prefetch_mir(tcx),
            || {
                let _ = tcx.exported_non_generic_symbols(LOCAL_CRATE);
                let _ = tcx.exported_generic_symbols(LOCAL_CRATE);
            },
        );
    }

    let _prof_timer = tcx.prof.verbose_generic_activity("generate_crate_metadata");
    let encoder = opaque::FileEncoder::new(path)
        .unwrap_or_else(|err| tcx.dcx().emit_fatal(FailCreateFileEncoder { err }));
    let encode = || {
        with_encode_metadata_header(tcx, MetadataEncoding::CoarseFull { encoder }, |ecx| {
            // Encode all the entries and extra information in the crate,
            // culminating in the `CrateRoot` which points to all of it.
            let (root, _) = ecx.encode_crate_root();

            // Flush buffer to ensure backing file has the correct size.
            ecx.opaque.flush();
            // Record metadata size for self-profiling
            tcx.prof.artifact_size(
                "crate_metadata",
                "crate_metadata",
                ecx.opaque.file_handle().metadata().unwrap().len(),
            );

            root.position.get()
        });
    };

    // Perform metadata encoding inside a task, so the dep-graph can check if any encoded
    // information changes, and maybe reuse the work product.
    tcx.dep_graph.with_task(dep_node, tcx, encode, None);
}

impl<'a, 'tcx> EncodeContext<'a, 'tcx> {
    fn new(tcx: TyCtxt<'tcx>, encoding: MetadataEncoding<'a>) -> Self {
        let hygiene_ctxt = match &encoding {
            MetadataEncoding::RdrTrace { hygiene_ctxt, .. } => Arc::clone(hygiene_ctxt),
            MetadataEncoding::RdrProjection { hygiene, .. } => {
                Arc::new(HygieneEncodeContext::with_layout((*hygiene).clone()))
            }
            MetadataEncoding::CoarseFull { .. } | MetadataEncoding::CoarseStub { .. } => {
                Arc::new(HygieneEncodeContext::default())
            }
        };
        let source_map_files = tcx.sess.source_map().files();
        let source_file_cache = (Arc::clone(&source_map_files[0]), 0);
        drop(source_map_files);

        let recorded_records =
            FxIndexSet::with_capacity_and_hasher(PersistedRecord::ALL.len(), Default::default());
        Self {
            opaque: MetadataEncoder { encoding, records: Vec::new() },
            tcx,
            feat: tcx.features(),
            tables: Default::default(),
            span_layout: SpanLayout::default(),
            span_occurrence_counts: FxHashMap::default(),
            hygiene_occurrence_counts: FxHashMap::default(),
            recorded_records,
            lazy_state: LazyState::NoNode,
            span_shorthands: Default::default(),
            type_shorthands: Default::default(),
            predicate_shorthands: Default::default(),
            source_file_cache,
            interpret_allocs: Default::default(),
            source_file_layout: SourceFileLayout::default(),
            hygiene_ctxt,
            symbol_index_table: Default::default(),
        }
    }
}

impl EncodeContext<'_, '_> {
    fn verify_record_coverage(&self) {
        let missing_records: Vec<_> = PersistedRecord::ALL
            .iter()
            .copied()
            .filter(|record| self.opaque.artifact_kind().contains(*record))
            .filter(|record| !self.recorded_records.contains(record))
            .map(PersistedRecord::name)
            .collect();
        assert!(
            missing_records.is_empty(),
            "metadata encoder did not write declared records: {missing_records:?}"
        );

        let unexpected_records: Vec<_> = self
            .recorded_records
            .iter()
            .copied()
            .filter(|record| !self.opaque.artifact_kind().contains(*record))
            .map(PersistedRecord::name)
            .collect();
        assert!(
            unexpected_records.is_empty(),
            "metadata encoder wrote records not declared for this artifact: {unexpected_records:?}"
        );
    }
}

fn with_encode_metadata_header<'a, 'tcx>(
    tcx: TyCtxt<'tcx>,
    encoding: MetadataEncoding<'a>,
    f: impl FnOnce(&mut EncodeContext<'a, 'tcx>) -> usize,
) {
    let mut ecx = EncodeContext::new(tcx, encoding);
    ecx.encode_preamble();
    ecx.with_record(PersistedRecord::Artifact(PersistedArtifactRecord::rustc_version), |encoder| {
        rustc_version(tcx.sess.cfg_version).encode(encoder)
    });
    let root_position = f(&mut ecx);
    ecx.verify_record_coverage();

    let MetadataEncoder { encoding, records: _ } = ecx.opaque;
    let mut encoder = match encoding {
        MetadataEncoding::CoarseFull { encoder } | MetadataEncoding::CoarseStub { encoder } => {
            encoder
        }
        MetadataEncoding::RdrTrace { .. } | MetadataEncoding::RdrProjection { .. } => {
            bug!("metadata projection reached file encoding")
        }
    };
    encoder
        .finish()
        .unwrap_or_else(|(path, err)| tcx.dcx().emit_fatal(FailWriteFile { path: &path, err }));
    if let Err(err) = encode_root_position(encoder.file(), root_position) {
        tcx.dcx().emit_fatal(FailWriteFile { path: encoder.path(), err });
    }
}

fn encode_root_position(mut file: &File, position: usize) -> Result<(), std::io::Error> {
    let position_before_seek = file.stream_position()?;
    file.seek(std::io::SeekFrom::Start(METADATA_HEADER.len() as u64))?;
    file.write_all(&position.to_le_bytes())?;
    file.seek(std::io::SeekFrom::Start(position_before_seek))?;
    Ok(())
}

pub(crate) fn provide(providers: &mut Providers) {
    *providers =
        Providers {
            doc_link_resolutions: |tcx, def_id| {
                tcx.metadata_resolutions(()).0.doc_link_resolutions.get(&def_id).unwrap_or_else(
                    || span_bug!(tcx.def_span(def_id), "no resolutions for a doc link"),
                )
            },
            doc_link_traits_in_scope: |tcx, def_id| {
                tcx.metadata_resolutions(()).0.doc_link_traits_in_scope.get(&def_id).unwrap_or_else(
                    || span_bug!(tcx.def_span(def_id), "no traits in scope for a doc link"),
                )
            },
            metadata_attrs,
            metadata_contract_hash,
            metadata_decode_layout_id,
            metadata_definition_spans,
            metadata_projection,
            metadata_resolutions,

            ..*providers
        }
}

/// Build a textual representation of an unevaluated constant expression.
///
/// If the const expression is too complex, an underscore `_` is returned.
/// For const arguments, it's `{ _ }` to be precise.
/// This means that the output is not necessarily valid Rust code.
///
/// Currently, only
///
/// * literals (optionally with a leading `-`)
/// * unit `()`
/// * blocks (`{ … }`) around simple expressions and
/// * paths without arguments
///
/// are considered simple enough. Simple blocks are included since they are
/// necessary to disambiguate unit from the unit type.
/// This list might get extended in the future.
///
/// Without this censoring, in a lot of cases the output would get too large
/// and verbose. Consider `match` expressions, blocks and deeply nested ADTs.
/// Further, private and `doc(hidden)` fields of structs would get leaked
/// since HIR datatypes like the `body` parameter do not contain enough
/// semantic information for this function to be able to hide them –
/// at least not without significant performance overhead.
///
/// Whenever possible, prefer to evaluate the constant first and try to
/// use a different method for pretty-printing. Ideally this function
/// should only ever be used as a fallback.
pub fn rendered_const<'tcx>(tcx: TyCtxt<'tcx>, body: &hir::Body<'_>, def_id: LocalDefId) -> String {
    let value = body.value;

    #[derive(PartialEq, Eq)]
    enum Classification {
        Literal,
        Simple,
        Complex,
    }

    use Classification::*;

    fn classify(expr: &hir::Expr<'_>) -> Classification {
        match &expr.kind {
            hir::ExprKind::Unary(hir::UnOp::Neg, expr) => {
                if matches!(expr.kind, hir::ExprKind::Lit(_)) { Literal } else { Complex }
            }
            hir::ExprKind::Lit(_) => Literal,
            hir::ExprKind::Tup([]) => Simple,
            hir::ExprKind::Block(hir::Block { stmts: [], expr: Some(expr), .. }, _) => {
                if classify(expr) == Complex { Complex } else { Simple }
            }
            // Paths with a self-type or arguments are too “complex” following our measure since
            // they may leak private fields of structs (with feature `adt_const_params`).
            // Consider: `<Self as Trait<{ Struct { private: () } }>>::CONSTANT`.
            // Paths without arguments are definitely harmless though.
            hir::ExprKind::Path(hir::QPath::Resolved(_, hir::Path { segments, .. })) => {
                if segments.iter().all(|segment| segment.args.is_none()) { Simple } else { Complex }
            }
            // FIXME: Claiming that those kinds of QPaths are simple is probably not true if the Ty
            //        contains const arguments. Is there a *concise* way to check for this?
            hir::ExprKind::Path(hir::QPath::TypeRelative(..)) => Simple,
            _ => Complex,
        }
    }

    match classify(value) {
        // For non-macro literals, we avoid invoking the pretty-printer and use the source snippet
        // instead to preserve certain stylistic choices the user likely made for the sake of
        // legibility, like:
        //
        // * hexadecimal notation
        // * underscores
        // * character escapes
        //
        // FIXME: This passes through `-/*spacer*/0` verbatim.
        Literal
            if !value.span.from_expansion()
                && let Ok(snippet) = tcx.sess.source_map().span_to_snippet(value.span) =>
        {
            snippet
        }

        // Otherwise we prefer pretty-printing to get rid of extraneous whitespace, comments and
        // other formatting artifacts.
        Literal | Simple => id_to_string(&tcx, body.id().hir_id),

        // FIXME: Omit the curly braces if the enclosing expression is an array literal
        //        with a repeated element (an `ExprKind::Repeat`) as in such case it
        //        would not actually need any disambiguation.
        Complex => {
            if tcx.def_kind(def_id) == DefKind::AnonConst {
                "{ _ }".to_owned()
            } else {
                "_".to_owned()
            }
        }
    }
}
