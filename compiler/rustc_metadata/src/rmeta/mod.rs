use std::hash::Hash;
use std::marker::PhantomData;
use std::num::NonZero;

use decoder::LazyDecoder;
pub(crate) use decoder::{
    CrateMetadata, CrateNumMap, LoadedMetadata, MetadataBlob, MetadataBlobError, MetadataInput,
    SpansArtifactSource, TargetModifiers,
};
use def_path_hash_map::DefPathHashMapRef;
use encoder::EncodeContext;
pub use encoder::{EncodedMetadata, EncodedMetadataArtifacts, RdrArtifactPair, rendered_const};
pub(crate) use encoder::{EncodedMetadataFiles, encode_metadata};
pub(crate) use parameterized::ParameterizedOverTcx;
use rustc_abi::{FieldIdx, ReprOptions, VariantIdx};
use rustc_ast as ast;
#[cfg(test)]
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::{FxHashMap, FxIndexSet};
use rustc_data_structures::stable_hash::{
    StableCompare, StableHash as StableHashTrait, StableHashCtxt, StableHasher,
};
use rustc_data_structures::svh::Svh;
use rustc_hir as hir;
use rustc_hir::attrs::StrippedCfgItem;
use rustc_hir::def::{CtorKind, DefKind, DocLinkResMap, MacroKinds, Namespace, Res};
use rustc_hir::def_id::{
    CrateNum, DefId, DefIdMap, DefIndex, DefPathHash, LocalDefId, StableCrateId,
};
use rustc_hir::definitions::DefKey;
use rustc_hir::lang_items::LangItem;
use rustc_hir::{PreciseCapturingArgKind, attrs};
use rustc_index::bit_set::DenseBitSet;
use rustc_index::{Idx, IndexVec};
use rustc_macros::{
    BlobDecodable, Decodable, Encodable, LazyDecodable, MetadataEncodable, StableHash, TyDecodable,
    TyEncodable,
};
use rustc_middle::metadata::{
    AmbigModChild, DefinitionState, MetadataContractHash, MetadataDecodeLayoutId,
    MetadataSpanLayout, ModChild, SelectedDefinitions,
};
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrs;
use rustc_middle::middle::debugger_visualizer::DebuggerVisualizerFile;
use rustc_middle::middle::deduced_param_attrs::DeducedParamAttrs;
use rustc_middle::middle::exported_symbols::{ExportedSymbol, SymbolExportInfo};
use rustc_middle::middle::lib_features::FeatureStability;
use rustc_middle::middle::resolve_bound_vars::ObjectLifetimeDefault;
use rustc_middle::mir;
use rustc_middle::mir::ConstValue;
use rustc_middle::ty::fast_reject::SimplifiedType;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_middle::util::Providers;
use rustc_serialize::Encodable;
use rustc_serialize::opaque::MAGIC_END_BYTES;
use rustc_session::config::mitigation_coverage::DeniedPartialMitigation;
use rustc_session::config::{SymbolManglingVersion, TargetModifier};
use rustc_session::cstore::{CrateDepKind, ForeignModule, LinkagePreference, NativeLib};
use rustc_span::edition::Edition;
use rustc_span::hygiene::{ExpnIndex, HygieneIdentity, MacroKind, SyntaxContextKey};
use rustc_span::{self, ExpnData, ExpnHash, ExpnId, ExternalSpanSlot, Ident, Span, Symbol};
use rustc_target::spec::{PanicStrategy, TargetTuple};
use table::{FixedSizeEncoding, TableBuilder};

use crate::eii::EiiMapEncodedKeyValue;

mod decoder;
mod def_path_hash_map;
mod parameterized;
mod table;

pub(crate) fn rustc_version(cfg_version: &'static str) -> String {
    format!("rustc {cfg_version}")
}

/// Metadata encoding version.
/// N.B., increment this if you change the format of metadata such that
/// the rustc version can't be found to compare with `rustc_version()`.
const METADATA_VERSION: u8 = 10;

const METADATA_ENVELOPE_VERSION: u8 = 0;
const METADATA_ENVELOPE_LEN: usize = 8;
pub(crate) const METADATA_ROOT_POSITION_OFFSET: usize = METADATA_ENVELOPE_LEN;
const COARSE_METADATA_PAYLOAD_OFFSET: usize = METADATA_ENVELOPE_LEN + 8;
const RDR_METADATA_LENGTH_OFFSET: usize = METADATA_ENVELOPE_LEN + 8;
const RDR_METADATA_PAYLOAD_OFFSET: usize = METADATA_ENVELOPE_LEN + 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum MetadataFormatKind {
    Coarse = 0,
    RdrV1 = 1,
}

impl MetadataFormatKind {
    pub(crate) const fn payload_offset(self) -> usize {
        match self {
            Self::Coarse => COARSE_METADATA_PAYLOAD_OFFSET,
            Self::RdrV1 => RDR_METADATA_PAYLOAD_OFFSET,
        }
    }

    const fn header(self) -> [u8; METADATA_ENVELOPE_LEN] {
        [b'r', b'u', b's', b't', METADATA_ENVELOPE_VERSION, self as u8, 0, METADATA_VERSION]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataEnvelopeError {
    Truncated,
    InvalidMagic,
    UnsupportedEnvelopeVersion(u8),
    UnsupportedFormat(u8),
    UnsupportedMetadataVersion(u8),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ParsedMetadataEnvelope<T> {
    pub(crate) bytes: T,
    pub(crate) format: MetadataFormatKind,
}

pub(crate) fn parse_metadata_envelope<T>(
    bytes: T,
) -> Result<ParsedMetadataEnvelope<T>, MetadataEnvelopeError>
where
    T: std::ops::Deref,
    T::Target: AsRef<[u8]>,
{
    let bytes_ref = std::ops::Deref::deref(&bytes).as_ref();
    let Some(header) = bytes_ref.get(..METADATA_ENVELOPE_LEN) else {
        return Err(MetadataEnvelopeError::Truncated);
    };
    if &header[..4] != b"rust" {
        return Err(MetadataEnvelopeError::InvalidMagic);
    }
    if header[4] != METADATA_ENVELOPE_VERSION {
        return Err(MetadataEnvelopeError::UnsupportedEnvelopeVersion(header[4]));
    }
    let format = match header[5] {
        0 => MetadataFormatKind::Coarse,
        1 => MetadataFormatKind::RdrV1,
        format => return Err(MetadataEnvelopeError::UnsupportedFormat(format)),
    };
    if header[7] != METADATA_VERSION {
        return Err(MetadataEnvelopeError::UnsupportedMetadataVersion(header[7]));
    }

    Ok(ParsedMetadataEnvelope { bytes, format })
}

/// Header emitted for coarse metadata.
///
/// Standalone metadata and embedded wrappers interpret the following word
/// differently, but share this envelope so both paths use the same format
/// dispatch.
pub const METADATA_HEADER: &[u8] = &MetadataFormatKind::Coarse.header();

/// A value of type T referred to by its absolute position
/// in the metadata, and which can be decoded lazily.
///
/// Metadata is effective a tree, encoded in post-order,
/// and with the root's position written next to the header.
/// That means every single `LazyValue` points to some previous
/// location in the metadata and is part of a larger node.
///
/// The first `LazyValue` in a node is encoded as the backwards
/// distance from the position where the containing node
/// starts and where the `LazyValue` points to, while the rest
/// use the forward distance from the previous `LazyValue`.
/// Distances start at 1, as 0-byte nodes are invalid.
/// Also invalid are nodes being referred in a different
/// order than they were encoded in.
#[must_use]
struct LazyValue<T> {
    position: NonZero<usize>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> LazyValue<T> {
    fn from_position(position: NonZero<usize>) -> LazyValue<T> {
        LazyValue { position, _marker: PhantomData }
    }
}

/// A list of lazily-decoded values.
///
/// Unlike `LazyValue<Vec<T>>`, the length is encoded next to the
/// position, not at the position, which means that the length
/// doesn't need to be known before encoding all the elements.
///
/// If the length is 0, no position is encoded, but otherwise,
/// the encoding is that of `LazyArray`, with the distinction that
/// the minimal distance the length of the sequence, i.e.
/// it's assumed there's no 0-byte element in the sequence.
struct LazyArray<T> {
    position: NonZero<usize>,
    num_elems: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Default for LazyArray<T> {
    fn default() -> LazyArray<T> {
        LazyArray::from_position_and_num_elems(NonZero::new(1).unwrap(), 0)
    }
}

impl<T> LazyArray<T> {
    fn from_position_and_num_elems(position: NonZero<usize>, num_elems: usize) -> LazyArray<T> {
        LazyArray { position, num_elems, _marker: PhantomData }
    }
}

/// A list of lazily-decoded values, with the added capability of random access.
///
/// Random-access table (i.e. offering constant-time `get`/`set`), similar to
/// `LazyArray<T>`, but without requiring encoding or decoding all the values
/// eagerly and in-order.
pub(crate) struct LazyTable<I, T> {
    position: NonZero<usize>,
    /// The encoded size of the elements of a table is selected at runtime to drop
    /// trailing zeroes. This is the number of bytes used for each table element.
    width: usize,
    /// How many elements are in the table.
    len: usize,
    _marker: PhantomData<fn(I) -> T>,
}

impl<I, T> Default for LazyTable<I, T> {
    fn default() -> Self {
        Self::from_position_and_encoded_size(NonZero::new(1).unwrap(), 0, 0)
    }
}

impl<I, T> LazyTable<I, T> {
    fn from_position_and_encoded_size(
        position: NonZero<usize>,
        width: usize,
        len: usize,
    ) -> LazyTable<I, T> {
        LazyTable { position, width, len, _marker: PhantomData }
    }
}

impl<T> Copy for LazyValue<T> {}
impl<T> Clone for LazyValue<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for LazyArray<T> {}
impl<T> Clone for LazyArray<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I, T> Copy for LazyTable<I, T> {}
impl<I, T> Clone for LazyTable<I, T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Values whose stable hash describes cross-artifact metadata meaning.
pub(crate) trait MetadataSemanticValue {}

macro_rules! impl_metadata_semantic_value {
    ($($ty:ty),+ $(,)?) => {
        $(impl MetadataSemanticValue for $ty {})+
    };
}

impl_metadata_semantic_value! {
    (),
    bool,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    char,
    str,
    String,
}

impl MetadataSemanticValue for CrateNum {}
impl MetadataSemanticValue for DefId {}
impl MetadataSemanticValue for LocalDefId {}
impl MetadataSemanticValue for Span {}
impl MetadataSemanticValue for Symbol {}
impl MetadataSemanticValue for rustc_span::ByteSymbol {}
impl MetadataSemanticValue for rustc_span::SyntaxContext {}
impl MetadataSemanticValue for ExpnId {}
impl MetadataSemanticValue for ExpnHash {}
impl MetadataSemanticValue for HygieneIdentity {}
impl MetadataSemanticValue for rustc_span::hygiene::LocalExpnId {}
impl MetadataSemanticValue for rustc_middle::mir::interpret::AllocId {}
impl<T: MetadataSemanticValue + ?Sized> MetadataSemanticValue for &T {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for Box<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for Option<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for Vec<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for [T] {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for &ty::List<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for &ty::ListWithCachedTypeInfo<T> {}
impl MetadataSemanticValue for Ty<'_> {}
impl MetadataSemanticValue for ty::Const<'_> {}
impl MetadataSemanticValue for ty::Region<'_> {}
impl MetadataSemanticValue for ty::Predicate<'_> {}
impl MetadataSemanticValue for ty::Clause<'_> {}
impl MetadataSemanticValue for ty::GenericArg<'_> {}
impl MetadataSemanticValue for ty::RestrictionKind {}
impl MetadataSemanticValue for ty::Term<'_> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for ty::Binder<'_, T> {}

macro_rules! impl_metadata_semantic_tuple {
    ($($name:ident),+) => {
        impl<$($name: MetadataSemanticValue),+> MetadataSemanticValue for ($($name,)+) {}
    };
}

impl_metadata_semantic_tuple!(A);
impl_metadata_semantic_tuple!(A, B);
impl_metadata_semantic_tuple!(A, B, C);
impl_metadata_semantic_tuple!(A, B, C, D);

impl_metadata_semantic_value! {
    CrateHeaderRecord,
    CrateDep,
    DefPathHash,
    StableCrateId,
    ReprOptions,
    VariantIdx,
    hir::Attribute,
    hir::Constness,
    hir::ConstStability,
    hir::CoroutineKind,
    hir::DefaultBodyStability,
    hir::Defaultness,
    hir::LangItem,
    hir::Safety,
    hir::Stability,
    hir::attrs::EiiDecl,
    hir::attrs::EiiImpl,
    hir::def::CtorKind,
    hir::def::DefKind,
    Namespace,
    AmbigModChild,
    ModChild,
    CodegenFnAttrs,
    DebuggerVisualizerFile,
    DeducedParamAttrs,
    FeatureStability,
    ObjectLifetimeDefault,
    mir::ConstQualifs,
    mir::ConstValue,
    ty::AnonConstKind,
    ty::AssocContainer,
    ty::AsyncDestructor,
    ty::Asyncness,
    ty::Destructor,
    ty::Generics,
    ty::ImplTraitInTraitData,
    ty::IntrinsicDef,
    ty::TraitDef,
    ty::Variance,
    ast::DelimArgs,
    attrs::Deprecation,
    DeniedPartialMitigation,
    SymbolManglingVersion,
    TargetModifier,
    rustc_span::edition::Edition,
    rustc_span::Ident,
    rustc_span::hygiene::Transparency,
    PanicStrategy,
    ForeignModule,
    NativeLib,
}

impl<T: MetadataSemanticValue> MetadataSemanticValue for hir::OpaqueTyOrigin<T> {}
impl<A: MetadataSemanticValue, B: MetadataSemanticValue> MetadataSemanticValue
    for PreciseCapturingArgKind<A, B>
{
}
impl<T: MetadataSemanticValue> MetadataSemanticValue for StrippedCfgItem<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for ty::EarlyBinder<'_, T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for ty::Visibility<T> {}
impl MetadataSemanticValue for SimplifiedType {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for rustc_data_structures::unord::UnordBag<T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for IndexVec<FieldIdx, T> {}
impl<T: MetadataSemanticValue> MetadataSemanticValue for IndexVec<mir::Promoted, T> {}
impl MetadataSemanticValue for DenseBitSet<u32> {}
impl MetadataSemanticValue for mir::Body<'_> {}
impl MetadataSemanticValue for mir::CoroutineLayout<'_> {}
impl MetadataSemanticValue for mir::interpret::ConstAllocation<'_> {}
impl MetadataSemanticValue for ty::ConstConditions<'_> {}
impl MetadataSemanticValue for ty::FnSig<'_> {}
impl MetadataSemanticValue for ty::GenericClauses<'_> {}
impl MetadataSemanticValue for ty::ImplTraitHeader<'_> {}
impl MetadataSemanticValue for ty::TraitRef<'_> {}
impl MetadataSemanticValue for ty::VariantDiscr {}
impl MetadataSemanticValue for ty::adjustment::CoerceUnsizedInfo {}
impl MetadataSemanticValue for ExportedSymbol<'_> {}
impl MetadataSemanticValue for SymbolExportInfo {}
impl MetadataSemanticValue for rustc_middle::middle::dependency_format::Linkage {}
impl MetadataSemanticValue for rustc_hir::def_id::LocalModId {}
impl MetadataSemanticValue for Res<!> {}

/// Encoding / decoding state for `Lazy`s (`LazyValue`, `LazyArray`, and `LazyTable`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum LazyState {
    /// Outside of a metadata node.
    NoNode,

    /// Inside a metadata node, and before any `Lazy`s.
    /// The position is that of the node itself.
    NodeStart(NonZero<usize>),

    /// Inside a metadata node, with a previous `Lazy`s.
    /// The position is where that previous `Lazy` would start.
    Previous(NonZero<usize>),
}

type SyntaxContextTable = LazyTable<u32, Option<LazyValue<SyntaxContextKey>>>;
type ExpnDataTable = LazyTable<ExpnIndex, Option<LazyValue<ExpnData>>>;
type ExpnHashTable = LazyTable<ExpnIndex, Option<LazyValue<ExpnHash>>>;

#[derive(MetadataEncodable, LazyDecodable)]
pub(crate) struct ProcMacroData {
    proc_macro_decls_static: DefIndex,
    stability: Option<hir::Stability>,
    macros: LazyArray<(DefIndex, LazyValue<ProcMacroKind>)>,
}

#[derive(MetadataEncodable, LazyDecodable)]
pub enum ProcMacroKind {
    CustomDerive { trait_name: String, attributes: Vec<String> },
    Attr { name: String },
    Bang { name: String },
}

#[derive(Clone, Copy, MetadataEncodable, LazyDecodable)]
struct SpansArtifactRoot {
    spans: LazyTable<ExternalSpanSlot, Option<LazyValue<MetadataSpanPosition>>>,
}

#[derive(Clone, Copy)]
struct MetadataSpanPosition {
    lo: rustc_span::BytePos,
    hi: rustc_span::BytePos,
}

#[derive(Clone, Copy)]
struct SpansArtifactHeader {
    root_position: NonZero<usize>,
}

impl SpansArtifactHeader {
    const FORMAT: [u8; 4] = *b"RSP1";
    const FORMAT_END: usize = Self::FORMAT.len();
    const ROOT_POSITION_END: usize = Self::FORMAT_END + size_of::<u64>();
    const ENCODED_LEN: usize = Self::ROOT_POSITION_END;

    fn parse(metadata_len: usize, spans: &[u8]) -> Option<Self> {
        if !spans.ends_with(MAGIC_END_BYTES) {
            return None;
        }
        let encoded_len = spans.len().checked_sub(MAGIC_END_BYTES.len())?;
        let header_start = encoded_len.checked_sub(Self::ENCODED_LEN)?;
        let header = spans.get(header_start..encoded_len)?;
        if header[..Self::FORMAT_END] != Self::FORMAT {
            return None;
        }
        let root_position = usize::try_from(u64::from_le_bytes(
            header[Self::FORMAT_END..Self::ROOT_POSITION_END].try_into().unwrap(),
        ))
        .ok()?;
        let payload_end = metadata_len.checked_add(header_start)?;
        if root_position < metadata_len || root_position >= payload_end {
            return None;
        }
        Some(Self { root_position: NonZero::new(root_position)? })
    }

    fn to_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let mut bytes = [0; Self::ENCODED_LEN];
        bytes[..Self::FORMAT_END].copy_from_slice(&Self::FORMAT);
        bytes[Self::FORMAT_END..].copy_from_slice(
            &u64::try_from(self.root_position.get())
                .expect("metadata spans root position exceeds u64")
                .to_le_bytes(),
        );
        bytes
    }
}

/// Serialized crate metadata.
///
/// This contains just enough information to determine if we should load the `CrateRoot` or not.
/// Prefer [`CrateRoot`] whenever possible to avoid ICEs when using `omit-git-hash` locally.
/// See #76720 for more details.
///
/// If you do modify this struct, also bump the [`METADATA_VERSION`] constant.
#[derive(MetadataEncodable, BlobDecodable)]
pub(crate) struct CrateHeader {
    pub(crate) triple: TargetTuple,
    pub(crate) hash: MetadataContractHash,
    pub(crate) name: Symbol,
    /// Whether this is the header for a proc-macro crate.
    ///
    /// This is separate from [`ProcMacroData`] to avoid having to update [`METADATA_VERSION`] every
    /// time ProcMacroData changes.
    pub(crate) is_proc_macro_crate: bool,
    /// Whether this crate metadata section is just a stub.
    /// Stubs do not contain the full metadata (it will be typically stored
    /// in a separate rmeta file).
    ///
    /// This is used inside rlibs and dylibs when using `-Zembed-metadata=no`.
    pub(crate) is_stub: bool,
}

#[derive(StableHash)]
struct CrateHeaderRecord {
    triple: TargetTuple,
    #[stable_hash(ignore)]
    hash: MetadataContractHash,
    name: Symbol,
    is_proc_macro_crate: bool,
    is_stub: bool,
}

#[derive(StableHash)]
struct DefKeyRecord {
    hash: DefPathHash,
    #[stable_hash(ignore)]
    key: DefKey,
}

impl MetadataSemanticValue for DefKeyRecord {}

#[derive(StableHash)]
struct CanonicalDefIdMap<'tcx, V> {
    semantic: &'tcx DefIdMap<V>,
    #[stable_hash(ignore)]
    encoded: Vec<(DefId, V)>,
}

impl<'tcx, V: Clone> CanonicalDefIdMap<'tcx, V> {
    fn new(tcx: TyCtxt<'tcx>, semantic: &'tcx DefIdMap<V>) -> Self {
        let encoded = semantic
            .items()
            .map(|(&def_id, value)| (tcx.def_path_hash(def_id), def_id, value.clone()))
            .into_sorted_stable_ord_by_key(|(hash, _, _)| hash)
            .into_iter()
            .map(|(_, def_id, value)| (def_id, value))
            .collect();
        Self { semantic, encoded }
    }
}

impl<V: MetadataSemanticValue> MetadataSemanticValue for CanonicalDefIdMap<'_, V> {}

struct MetadataExpnData(ExpnData);

impl StableHashTrait for MetadataExpnData {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.0.stable_hash_artifact_semantics(hcx, hasher);
    }
}

impl MetadataSemanticValue for MetadataExpnData {}

#[derive(StableHash)]
struct DocLinkResolutionsRecord {
    semantic: rustc_data_structures::unord::UnordMap<(Symbol, Namespace), Option<Res<!>>>,
    #[stable_hash(ignore)]
    encoded: Vec<DocLinkResolution>,
}

struct DocLinkResolution {
    symbol: Symbol,
    namespace: Namespace,
    resolution: Option<Res<ast::NodeId>>,
}

impl StableCompare for DocLinkResolution {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    fn stable_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.symbol
            .as_str()
            .cmp(other.symbol.as_str())
            .then_with(|| self.namespace.cmp(&other.namespace))
    }
}

impl DocLinkResolutionsRecord {
    fn new(resolutions: &DocLinkResMap) -> Self {
        let mut encoded: Vec<_> = resolutions
            .iter()
            .map(|(&(symbol, namespace), &resolution)| DocLinkResolution {
                symbol,
                namespace,
                resolution,
            })
            .collect();
        encoded.sort_unstable_by(|a, b| a.stable_cmp(b));
        let semantic = encoded
            .iter()
            .map(|entry| {
                (
                    (entry.symbol, entry.namespace),
                    entry.resolution.map(|resolution| resolution.expect_non_local()),
                )
            })
            .collect();
        Self { semantic, encoded }
    }
}

impl MetadataSemanticValue for DocLinkResolutionsRecord {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistedProjection {
    Semantic,
    DecodeLayout,
    Position,
    SelfIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PersistedProjections(NonZero<u8>);

impl PersistedProjections {
    const fn new(projection: PersistedProjection) -> Self {
        Self(NonZero::new(1 << projection as u8).unwrap())
    }

    const fn with(self, projection: PersistedProjection) -> Self {
        Self(NonZero::new(self.0.get() | 1 << projection as u8).unwrap())
    }

    const fn contains(self, projection: PersistedProjection) -> bool {
        self.0.get() & 1 << projection as u8 != 0
    }
}

macro_rules! persisted_projections {
    ($first:ident $(, $rest:ident)*) => {
        PersistedProjections::new(PersistedProjection::$first)
            $(.with(PersistedProjection::$rest))*
    };
}

macro_rules! define_artifact_records {
    ($($record:ident in [$($artifact:ident),+] => [$($projection:ident),+],)+) => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(u8)]
        pub(crate) enum PersistedArtifactRecord {
            $($record,)+
        }

        impl PersistedArtifactRecord {
            const fn name(self) -> &'static str {
                match self {
                    $(Self::$record => stringify!($record),)+
                }
            }

            const fn projections(self) -> PersistedProjections {
                match self {
                    $(
                        Self::$record => persisted_projections!($($projection),+),
                    )+
                }
            }

            const fn artifacts(self) -> &'static [MetadataArtifactKind] {
                match self {
                    $(Self::$record => &[$(MetadataArtifactKind::$artifact,)+],)+
                }
            }
        }
    }
}

macro_rules! define_root_writer {
    ($field:ident: $field_type:ty, [$($projection:ident),+ $(,)?]) => {
        define_root_writer!(@contains_semantic $field: $field_type, [$($projection),+]);
    };
    (@contains_semantic $field:ident: $field_type:ty, [Semantic $(, $rest:ident)*]) => {
        fn $field<L: MetadataSemanticValue + StableHashTrait, E>(
            &mut self,
            logical: L,
            encode: impl FnOnce(&mut Self, L) -> E,
        ) -> E {
            self.with_record(
                PersistedRecord::CrateRoot(PersistedCrateRootField::$field),
                |encoder| {
                    encoder.project_semantic(&logical);
                    encode(encoder, logical)
                },
            )
        }
    };
    (
        @contains_semantic $field:ident: $field_type:ty,
        [$projection:ident $(, $rest:ident)*]
    ) => {
        define_root_writer!(@contains_semantic $field: $field_type, [$($rest),*]);
    };
    (@contains_semantic $field:ident: $field_type:ty, []) => {
        fn $field<L, E>(
            &mut self,
            logical: L,
            encode: impl FnOnce(&mut Self, L) -> E,
        ) -> E {
            self.with_record(
                PersistedRecord::CrateRoot(PersistedCrateRootField::$field),
                |encoder| encode(encoder, logical),
            )
        }
    };
}

macro_rules! define_crate_root {
    (
        $(#[$attr:meta])*
        $visibility:vis struct $root:ident {
            $(
                $(#[$field_attr:meta])*
                $field_visibility:vis $field:ident: $field_type:ty
                    => [$($projection:ident),+] position_by $position_owner:ident,
            )+
        }
    ) => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(u8)]
        pub(crate) enum PersistedCrateRootField {
            $($field,)+
        }

        impl PersistedCrateRootField {
            const fn name(self) -> &'static str {
                match self {
                    $(Self::$field => stringify!($field),)+
                }
            }

        }

        $(#[$attr])*
        $visibility struct $root {
            $(
                $(#[$field_attr])*
                $field_visibility $field: $field_type,
            )+
        }

        impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for $root {
            fn encode(&self, encoder: &mut EncodeContext<'a, 'tcx>) {
                $(
                    encoder.with_record(
                        PersistedRecord::CrateRoot(PersistedCrateRootField::$field),
                        |encoder| self.$field.encode(encoder),
                    );
                )+
            }
        }

        macro_rules! define_root_writers {
            () => {
                impl<'a, 'tcx> EncodeContext<'a, 'tcx> {
                    $(
                        define_root_writer!(
                            $field: $field_type,
                            [$($projection),+]
                        );
                    )+
                }
            }
        }
    }
}

/// Stores a `DefId` in the metadata artifact's numbering spaces.
///
/// Construction remaps local definition indexes immediately so fixed-size tables cannot bypass
/// RDR reference discovery.
#[derive(Copy, Clone, LazyDecodable)]
pub(crate) struct RawDefId {
    krate: u32,
    index: u32,
}

impl RawDefId {
    fn new(encoder: &mut EncodeContext<'_, '_>, def_id: DefId) -> Self {
        let index = match def_id.as_local() {
            Some(_) => encoder.map_def_index(def_id.index),
            None => def_id.index,
        };
        Self { krate: def_id.krate.as_u32(), index: index.as_u32() }
    }

    fn decode(self, meta: (&CrateMetadata, TyCtxt<'_>)) -> DefId {
        let krate = CrateNum::from_u32(self.krate);
        let krate = meta.0.map_encoded_cnum_to_current(krate);
        DefId { krate, index: DefIndex::from_u32(self.index) }
    }
}

impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for RawDefId {
    fn encode(&self, encoder: &mut EncodeContext<'a, 'tcx>) {
        CrateNum::from_u32(self.krate).encode(encoder);
        self.index.encode(encoder);
    }
}

#[derive(Encodable, BlobDecodable, StableHash)]
pub(crate) struct CrateDep {
    pub name: Symbol,
    pub hash: Svh,
    pub host_hash: Option<Svh>,
    pub kind: CrateDepKind,
    pub extra_filename: String,
    pub is_private: bool,
}

#[derive(MetadataEncodable, LazyDecodable)]
pub(crate) struct TraitImpls {
    trait_id: RawDefId,
    impls: LazyArray<(DefIndex, Option<SimplifiedType>)>,
}

#[derive(MetadataEncodable, LazyDecodable)]
pub(crate) struct IncoherentImpls {
    self_ty: LazyValue<SimplifiedType>,
    impls: LazyArray<DefIndex>,
}

pub(crate) struct DeclaredTable<I: Idx, T: FixedSizeEncoding>(TableBuilder<I, T>);

impl<I: Idx, T: FixedSizeEncoding> Default for DeclaredTable<I, T> {
    fn default() -> Self {
        Self(TableBuilder::default())
    }
}

impl<I: Idx, const N: usize, T> DeclaredTable<I, Option<T>>
where
    Option<T>: FixedSizeEncoding<ByteArray = [u8; N]>,
{
    fn set_some(&mut self, index: I, value: T) {
        self.0.set_some(index, value);
    }
}

impl<I: Idx, const N: usize, T> DeclaredTable<I, T>
where
    T: FixedSizeEncoding<ByteArray = [u8; N]>,
{
    fn set(&mut self, index: I, value: T) {
        self.0.set(index, value);
    }

    fn encode(&self, position: usize, emit: impl FnMut(&[u8])) -> LazyTable<I, T> {
        self.0.encode(position, emit)
    }
}

/// Define `LazyTables` and `TableBuilders` at the same time.
macro_rules! define_tables {
    (
        - definition:
            - defaulted:
                $(
                    $definition_defaulted:ident: Table<DefIndex, $definition_defaulted_value:ty>
                        => [$($definition_defaulted_projection:ident),+]
                            position_by $definition_defaulted_position_owner:ident,
                )+
            - optional:
                $(
                    $definition_optional:ident: Table<DefIndex, $definition_optional_value:ty>
                        => [$($definition_optional_projection:ident),+]
                            position_by $definition_optional_position_owner:ident,
                )+
        - ordinal:
            - optional:
                $(
                    $ordinal_optional:ident: Table<$ordinal_index:ty, $ordinal_optional_value:ty>
                        => [$($ordinal_optional_projection:ident),+]
                            position_by $ordinal_optional_position_owner:ident,
                )+
    ) => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(u8)]
        pub(crate) enum PersistedTable {
            $($definition_defaulted,)+
            $($definition_optional,)+
            $($ordinal_optional,)+
        }

        impl PersistedTable {
            const fn name(self) -> &'static str {
                match self {
                    $(
                        Self::$definition_defaulted => stringify!($definition_defaulted),
                    )+
                    $(
                        Self::$definition_optional => stringify!($definition_optional),
                    )+
                    $(
                        Self::$ordinal_optional => stringify!($ordinal_optional),
                    )+
                }
            }

        }

        #[derive(LazyDecodable)]
        pub(crate) struct LazyTables {
            $(
                $definition_defaulted:
                    LazyTable<DefIndex, $definition_defaulted_value>,
            )+
            $(
                $definition_optional:
                    LazyTable<DefIndex, Option<$definition_optional_value>>,
            )+
            $(
                $ordinal_optional:
                    LazyTable<$ordinal_index, Option<$ordinal_optional_value>>,
            )+
        }

        impl<'a, 'tcx> Encodable<EncodeContext<'a, 'tcx>> for LazyTables {
            fn encode(&self, encoder: &mut EncodeContext<'a, 'tcx>) {
                $(
                    encoder.with_record(
                        PersistedRecord::Table(PersistedTable::$definition_defaulted),
                        |encoder| self.$definition_defaulted.encode(encoder),
                    );
                )+
                $(
                    encoder.with_record(
                        PersistedRecord::Table(PersistedTable::$definition_optional),
                        |encoder| self.$definition_optional.encode(encoder),
                    );
                )+
                $(
                    encoder.with_record(
                        PersistedRecord::Table(PersistedTable::$ordinal_optional),
                        |encoder| self.$ordinal_optional.encode(encoder),
                    );
                )+
            }
        }

        #[derive(Default)]
        struct TableBuilders {
            $(
                $definition_defaulted:
                    DeclaredTable<DefIndex, $definition_defaulted_value>,
            )+
            $(
                $definition_optional:
                    DeclaredTable<DefIndex, Option<$definition_optional_value>>,
            )+
            $(
                $ordinal_optional:
                    DeclaredTable<$ordinal_index, Option<$ordinal_optional_value>>,
            )+
        }

        impl TableBuilders {
            fn encode<'a, 'tcx>(
                &self,
                encoder: &mut EncodeContext<'a, 'tcx>,
            ) -> LazyTables {
                LazyTables {
                    $(
                        $definition_defaulted: encoder.encode_table(
                            PersistedTable::$definition_defaulted,
                            &self.$definition_defaulted,
                        ),
                    )+
                    $(
                        $definition_optional: encoder.encode_table(
                            PersistedTable::$definition_optional,
                            &self.$definition_optional,
                        ),
                    )+
                    $(
                        $ordinal_optional: encoder.encode_table(
                            PersistedTable::$ordinal_optional,
                            &self.$ordinal_optional,
                        ),
                    )+
                }
            }
        }

        macro_rules! define_table_writers {
            () => {
                impl<'a, 'tcx> EncodeContext<'a, 'tcx> {
                    $(
                        fn $definition_defaulted<L: MetadataSemanticValue + StableHashTrait>(
                            &mut self,
                            index: DefIndex,
                            value: L,
                            encode: fn(&mut Self, L) -> $definition_defaulted_value,
                        ) {
                            let record = PersistedRecord::Table(
                                PersistedTable::$definition_defaulted,
                            );
                            if !self.visits(record) {
                                return;
                            }
                            self.with_definition_record(
                                record,
                                index,
                                |encoder| {
                                    encoder.project_semantic(&value);
                                    let value = encode(encoder, value);
                                    let index = encoder.map_def_index(index);
                                    encoder.tables.$definition_defaulted.set(index, value);
                                },
                            );
                        }
                    )+

                    $(
                        fn $definition_optional<L: MetadataSemanticValue + StableHashTrait>(
                            &mut self,
                            index: DefIndex,
                            value: L,
                            encode: fn(&mut Self, L) -> $definition_optional_value,
                        ) {
                            let record = PersistedRecord::Table(
                                PersistedTable::$definition_optional,
                            );
                            if !self.visits(record) {
                                return;
                            }
                            self.with_definition_record(
                                record,
                                index,
                                |encoder| {
                                    encoder.project_semantic(&value);
                                    let value = encode(encoder, value);
                                    let index = encoder.map_def_index(index);
                                    encoder.tables.$definition_optional.set_some(index, value);
                                },
                            );
                        }
                    )+

                    $(
                        fn $ordinal_optional<L: MetadataSemanticValue + StableHashTrait>(
                            &mut self,
                            index: $ordinal_index,
                            value: L,
                            encode: fn(&mut Self, L) -> $ordinal_optional_value,
                        ) {
                            let record = PersistedRecord::Table(
                                PersistedTable::$ordinal_optional,
                            );
                            if !self.visits(record) {
                                return;
                            }
                            self.with_record(
                                record,
                                |encoder| {
                                    encoder.project_semantic(&value);
                                    let value = encode(encoder, value);
                                    encoder.tables.$ordinal_optional.set_some(index, value);
                                },
                            );
                        }
                    )+
                }
            }
        }
    }
}

macro_rules! define_metadata_schema {
    (
        - artifacts:
            $(
                $artifact:ident in [$($artifact_kind:ident),+]
                    => [$($artifact_projection:ident),+],
            )+
        - position-passes:
            $($position_pass:ident,)+
        - root:
            $(#[$root_attr:meta])*
            $root_visibility:vis struct $root:ident {
                $(
                    $(#[$field_attr:meta])*
                    $field_visibility:vis $field:ident: $field_type:ty
                        => [$($field_projection:ident),+]
                            position_by $field_position_owner:ident,
                )+
            }
        - tables:
            - definition:
                - defaulted:
                    $(
                        $definition_defaulted:ident:
                            Table<DefIndex, $definition_defaulted_value:ty>
                            => [$($definition_defaulted_projection:ident),+]
                                position_by $definition_defaulted_position_owner:ident,
                    )+
                - optional:
                    $(
                        $definition_optional:ident:
                            Table<DefIndex, $definition_optional_value:ty>
                            => [$($definition_optional_projection:ident),+]
                                position_by $definition_optional_position_owner:ident,
                    )+
            - ordinal:
                - optional:
                    $(
                        $ordinal_optional:ident:
                            Table<$ordinal_index:ty, $ordinal_optional_value:ty>
                            => [$($ordinal_optional_projection:ident),+]
                                position_by $ordinal_optional_position_owner:ident,
                    )+
    ) => {
        define_artifact_records! {
            $(
                $artifact in [$($artifact_kind),+]
                    => [$($artifact_projection),+],
            )+
        }

        define_crate_root! {
            $(#[$root_attr])*
            $root_visibility struct $root {
                $(
                    $(#[$field_attr])*
                    $field_visibility $field: $field_type
                        => [$($field_projection),+]
                            position_by $field_position_owner,
                )+
            }
        }

        define_tables! {
            - definition:
                - defaulted:
                    $(
                        $definition_defaulted: Table<DefIndex, $definition_defaulted_value>
                            => [$($definition_defaulted_projection),+]
                                position_by $definition_defaulted_position_owner,
                    )+
                - optional:
                    $(
                        $definition_optional: Table<DefIndex, $definition_optional_value>
                            => [$($definition_optional_projection),+]
                                position_by $definition_optional_position_owner,
                    )+
            - ordinal:
                - optional:
                    $(
                        $ordinal_optional: Table<$ordinal_index, $ordinal_optional_value>
                            => [$($ordinal_optional_projection),+]
                                position_by $ordinal_optional_position_owner,
                    )+
        }

        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) enum PersistedRecord {
            Artifact(PersistedArtifactRecord),
            CrateRoot(PersistedCrateRootField),
            Table(PersistedTable),
        }

        macro_rules! projections_for_position_owner {
            (none, $projections:expr) => {
                $projections
            };
            ($owner:ident, $projections:expr) => {
                $projections.with(PersistedProjection::Position)
            };
        }

        impl PersistedRecord {
            const ALL: &'static [Self] = &[
                $(Self::Artifact(PersistedArtifactRecord::$artifact),)+
                $(Self::CrateRoot(PersistedCrateRootField::$field),)+
                $(Self::Table(PersistedTable::$definition_defaulted),)+
                $(Self::Table(PersistedTable::$definition_optional),)+
                $(Self::Table(PersistedTable::$ordinal_optional),)+
            ];

            const fn projections(self) -> PersistedProjections {
                match self {
                    Self::Artifact(record) => record.projections(),
                    $(
                        Self::CrateRoot(PersistedCrateRootField::$field) => {
                            projections_for_position_owner!(
                                $field_position_owner,
                                persisted_projections!($($field_projection),+)
                            )
                        },
                    )+
                    $(
                        Self::Table(PersistedTable::$definition_defaulted) => {
                            projections_for_position_owner!(
                                $definition_defaulted_position_owner,
                                persisted_projections!(
                                    $($definition_defaulted_projection),+
                                )
                            )
                        },
                    )+
                    $(
                        Self::Table(PersistedTable::$definition_optional) => {
                            projections_for_position_owner!(
                                $definition_optional_position_owner,
                                persisted_projections!(
                                    $($definition_optional_projection),+
                                )
                            )
                        },
                    )+
                    $(
                        Self::Table(PersistedTable::$ordinal_optional) => {
                            projections_for_position_owner!(
                                $ordinal_optional_position_owner,
                                persisted_projections!(
                                    $($ordinal_optional_projection),+
                                )
                            )
                        },
                    )+
                }
            }
        }

        #[allow(non_camel_case_types)]
        #[repr(u8)]
        enum PositionPass {
            $($position_pass,)+
        }

        macro_rules! position_pass {
            (none) => {
                Option::<PositionPass>::None
            };
            ($owner:ident) => {
                Some(PositionPass::$owner)
            };
        }

        const _: () = {
            let mut used = [false; [$(PositionPass::$position_pass,)+].len()];
            $(
                if let Some(pass) = position_pass!($field_position_owner) {
                    used[pass as usize] = true;
                }
            )+
            $(
                if let Some(pass) = position_pass!($definition_defaulted_position_owner) {
                    used[pass as usize] = true;
                }
            )+
            $(
                if let Some(pass) = position_pass!($definition_optional_position_owner) {
                    used[pass as usize] = true;
                }
            )+
            $(
                if let Some(pass) = position_pass!($ordinal_optional_position_owner) {
                    used[pass as usize] = true;
                }
            )+
            let mut index = 0;
            while index < used.len() {
                assert!(used[index], "metadata position pass owns no records");
                index += 1;
            }
        };

        impl MetadataArtifactKind {
            fn contains(self, record: PersistedRecord) -> bool {
                match record {
                    PersistedRecord::Artifact(record) => record.artifacts().contains(&self),
                    PersistedRecord::CrateRoot(_) | PersistedRecord::Table(_) => match self {
                        Self::Full | Self::RdrMetadata => true,
                        Self::Spans | Self::Stub => false,
                    },
                }
            }
        }

        macro_rules! define_position_traversal {
            () => {
                impl EncodeContext<'_, '_> {
                    fn encode_position_records(&mut self) {
                        $(let _ = self.$position_pass();)+
                    }
                }
            }
        }
    }
}

define_metadata_schema! {
- artifacts:
    metadata_header in [Full, RdrMetadata, Stub] => [DecodeLayout],
    root_position in [Full, RdrMetadata, Stub] => [DecodeLayout],
    rustc_version in [Full, RdrMetadata, Stub] => [DecodeLayout],
    spans in [Full, Spans] => [Position],
    stub_crate_header in [Stub] => [Semantic, DecodeLayout],
- position-passes:
    encode_externally_implementable_items,
    encode_stripped_cfg_items,
    encode_native_libraries,
    encode_def_ids,
    encode_hygiene,
    encode_proc_macros,
- root:
/// Serialized `.rmeta` data for a crate.
///
/// When compiling a proc-macro crate, we encode many of
/// the `LazyArray<T>` fields as `Lazy::empty()`. This serves two purposes:
///
/// 1. We avoid performing unnecessary work. Proc-macro crates can only
/// export proc-macros functions, which are compiled into a shared library.
/// As a result, a large amount of the information we normally store
/// (e.g. optimized MIR) is unneeded by downstream crates.
/// 2. We avoid serializing invalid `CrateNum`s. When we deserialize
/// a proc-macro crate, we don't load any of its dependencies (since we
/// just need to invoke a native function from the shared library).
/// This means that any foreign `CrateNum`s that we serialize cannot be
/// deserialized, since we will not know how to map them into the current
/// compilation session. If we were to serialize a proc-macro crate like
/// a normal crate, much of what we serialized would be unusable in addition
/// to being unused.
#[derive(LazyDecodable)]
pub(crate) struct CrateRoot {
    /// A header used to detect if this is the right crate to load.
    header: CrateHeader => [Semantic] position_by none,
    metadata_decode_layout_id: MetadataDecodeLayoutId => [SelfIdentity] position_by none,

    extra_filename: String => [DecodeLayout] position_by none,
    stable_crate_id: StableCrateId => [Semantic, DecodeLayout] position_by none,
    required_panic_strategy: Option<PanicStrategy> => [Semantic] position_by none,
    panic_in_drop_strategy: PanicStrategy => [Semantic] position_by none,
    edition: Edition => [Semantic] position_by none,
    has_global_allocator: bool => [Semantic] position_by none,
    has_alloc_error_handler: bool => [Semantic] position_by none,
    has_panic_handler: bool => [Semantic] position_by none,
    has_default_lib_allocator: bool => [Semantic] position_by none,
    externally_implementable_items: LazyArray<EiiMapEncodedKeyValue>
        => [Semantic, DecodeLayout] position_by encode_externally_implementable_items,

    crate_deps: LazyArray<CrateDep> => [Semantic, DecodeLayout] position_by none,
    dylib_dependency_formats: LazyArray<Option<LinkagePreference>> => [Semantic, DecodeLayout] position_by none,
    lib_features: LazyArray<(Symbol, FeatureStability)> => [Semantic, DecodeLayout] position_by none,
    stability_implications: LazyArray<(Symbol, Symbol)> => [Semantic, DecodeLayout] position_by none,
    lang_items: LazyArray<(DefIndex, LangItem)> => [Semantic, DecodeLayout] position_by none,
    lang_items_missing: LazyArray<LangItem> => [Semantic, DecodeLayout] position_by none,
    stripped_cfg_items: LazyArray<StrippedCfgItem<DefIndex>>
        => [Semantic, DecodeLayout] position_by encode_stripped_cfg_items,
    diagnostic_items: LazyArray<(Symbol, DefIndex)> => [Semantic, DecodeLayout] position_by none,
    canonical_symbols: LazyArray<(Symbol, DefIndex)> => [Semantic, DecodeLayout] position_by none,
    native_libraries: LazyArray<NativeLib>
        => [Semantic, DecodeLayout] position_by encode_native_libraries,
    foreign_modules: LazyArray<ForeignModule> => [Semantic, DecodeLayout] position_by none,
    traits: LazyArray<DefIndex> => [Semantic, DecodeLayout] position_by none,
    impls: LazyArray<TraitImpls> => [Semantic, DecodeLayout] position_by none,
    incoherent_impls: LazyArray<IncoherentImpls> => [Semantic, DecodeLayout] position_by none,
    interpret_alloc_index: LazyArray<u64> => [DecodeLayout] position_by none,
    proc_macro_data: Option<ProcMacroData> => [DecodeLayout] position_by none,

    tables: LazyTables => [DecodeLayout] position_by none,
    debugger_visualizers: LazyArray<DebuggerVisualizerFile> => [Semantic, DecodeLayout] position_by none,

    exportable_items: LazyArray<DefIndex> => [Semantic, DecodeLayout] position_by none,
    stable_order_of_exportable_impls: LazyArray<(DefIndex, usize)> => [Semantic, DecodeLayout] position_by none,
    exported_non_generic_symbols: LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>
        => [Semantic, DecodeLayout] position_by none,
    exported_generic_symbols: LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>
        => [Semantic, DecodeLayout] position_by none,

    syntax_contexts: SyntaxContextTable
        => [Semantic, DecodeLayout] position_by encode_hygiene,
    expn_data: ExpnDataTable => [Semantic, DecodeLayout] position_by encode_hygiene,
    expn_hashes: ExpnHashTable => [Semantic, DecodeLayout] position_by none,

    def_path_hash_map: LazyValue<DefPathHashMapRef<'static>> => [DecodeLayout] position_by none,

    source_map: LazyTable<u32, Option<LazyValue<rustc_span::SourceFile>>> => [DecodeLayout] position_by none,
    target_modifiers: LazyArray<TargetModifier> => [Semantic, DecodeLayout] position_by none,
    denied_partial_mitigations: LazyArray<DeniedPartialMitigation> => [Semantic, DecodeLayout] position_by none,

    compiler_builtins: bool => [Semantic] position_by none,
    needs_allocator: bool => [Semantic] position_by none,
    needs_panic_runtime: bool => [Semantic] position_by none,
    no_builtins: bool => [Semantic] position_by none,
    panic_runtime: bool => [Semantic] position_by none,
    profiler_runtime: bool => [Semantic] position_by none,
    symbol_mangling_version: SymbolManglingVersion => [Semantic] position_by none,

    specialization_enabled_in: bool => [Semantic] position_by none,
}
- tables:
- definition:
- defaulted:
    intrinsic: Table<DefIndex, Option<LazyValue<ty::IntrinsicDef>>>
        => [Semantic, DecodeLayout] position_by none,
    is_macro_rules: Table<DefIndex, bool>
        => [Semantic, DecodeLayout] position_by none,
    type_alias_is_checked: Table<DefIndex, bool>
        => [Semantic, DecodeLayout] position_by none,
    attr_flags: Table<DefIndex, AttrFlags>
        => [Semantic, DecodeLayout] position_by none,
    // The u64 is the crate-local part of the DefPathHash. All hashes in this crate have the same
    // StableCrateId, so we omit encoding those into the table.
    //
    // Note also that this table is fully populated (no gaps) as every DefIndex should have a
    // corresponding DefPathHash.
    def_path_hashes: Table<DefIndex, u64>
        => [Semantic, DecodeLayout] position_by none,
    explicit_item_bounds: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    explicit_item_self_bounds: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    inferred_outlives_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    explicit_super_clauses_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    explicit_implied_clauses_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    explicit_implied_const_bounds: Table<DefIndex, LazyArray<(ty::PolyTraitRef<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    inherent_impls: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout] position_by none,
    opt_rpitit_info: Table<DefIndex, Option<LazyValue<ty::ImplTraitInTraitData>>>
        => [Semantic, DecodeLayout] position_by none,
    // Reexported names are not associated with individual `DefId`s,
    // e.g. a glob import can introduce a lot of names, all with the same `DefId`.
    // That's why the encoded list needs to contain `ModChild` structures describing all the names
    // individually instead of `DefId`s.
    module_children_reexports: Table<DefIndex, LazyArray<ModChild>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    ambig_module_children: Table<DefIndex, LazyArray<AmbigModChild>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    cross_crate_inlinable: Table<DefIndex, bool>
        => [Semantic, DecodeLayout] position_by none,
    asyncness: Table<DefIndex, ty::Asyncness>
        => [Semantic, DecodeLayout] position_by none,
    constness: Table<DefIndex, hir::Constness>
        => [Semantic, DecodeLayout] position_by none,
    safety: Table<DefIndex, hir::Safety>
        => [Semantic, DecodeLayout] position_by none,
    defaultness: Table<DefIndex, hir::Defaultness>
        => [Semantic, DecodeLayout] position_by none,
    impl_is_fully_generic_for_reflection: Table<DefIndex, bool>
        => [Semantic, DecodeLayout] position_by none,

- optional:
    attributes: Table<DefIndex, LazyArray<hir::Attribute>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    // For non-reexported names in a module every name is associated with a separate `DefId`,
    // so we can take their names, visibilities etc from other encoded tables.
    module_children_non_reexports: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout] position_by none,
    associated_item_or_field_def_ids: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout] position_by none,
    def_kind: Table<DefIndex, DefKind>
        => [Semantic, DecodeLayout] position_by none,
    visibility: Table<DefIndex, LazyValue<ty::Visibility<DefIndex>>>
        => [Semantic, DecodeLayout] position_by none,
    def_span: Table<DefIndex, LazyValue<Span>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    def_ident_span: Table<DefIndex, LazyValue<Span>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    lookup_stability: Table<DefIndex, LazyValue<hir::Stability>>
        => [Semantic, DecodeLayout] position_by none,
    lookup_const_stability: Table<DefIndex, LazyValue<hir::ConstStability>>
        => [Semantic, DecodeLayout] position_by none,
    lookup_default_body_stability: Table<DefIndex, LazyValue<hir::DefaultBodyStability>>
        => [Semantic, DecodeLayout] position_by none,
    lookup_deprecation_entry: Table<DefIndex, LazyValue<attrs::Deprecation>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    explicit_clauses_of: Table<DefIndex, LazyValue<ty::GenericClauses<'static>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    generics_of: Table<DefIndex, LazyValue<ty::Generics>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    type_of: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, Ty<'static>>>>
        => [Semantic, DecodeLayout] position_by none,
    variances_of: Table<DefIndex, LazyArray<ty::Variance>>
        => [Semantic, DecodeLayout] position_by none,
    fn_sig: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::PolyFnSig<'static>>>>
        => [Semantic, DecodeLayout] position_by none,
    codegen_fn_attrs: Table<DefIndex, LazyValue<CodegenFnAttrs>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    impl_trait_header: Table<DefIndex, LazyValue<ty::ImplTraitHeader<'static>>>
        => [Semantic, DecodeLayout] position_by none,
    const_param_default: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, rustc_middle::ty::Const<'static>>>>
        => [Semantic, DecodeLayout] position_by none,
    object_lifetime_default: Table<DefIndex, LazyValue<ObjectLifetimeDefault>>
        => [Semantic, DecodeLayout] position_by none,
    optimized_mir: Table<DefIndex, LazyValue<mir::Body<'static>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    mir_for_ctfe: Table<DefIndex, LazyValue<mir::Body<'static>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    trivial_const: Table<DefIndex, LazyValue<(ConstValue, Ty<'static>)>>
        => [Semantic, DecodeLayout] position_by none,
    closure_saved_names_of_captured_variables: Table<DefIndex, LazyValue<IndexVec<FieldIdx, Symbol>>>
        => [Semantic, DecodeLayout] position_by none,
    mir_coroutine_witnesses: Table<DefIndex, LazyValue<mir::CoroutineLayout<'static>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    promoted_mir: Table<DefIndex, LazyValue<IndexVec<mir::Promoted, mir::Body<'static>>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    thir_abstract_const: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::Const<'static>>>>
        => [Semantic, DecodeLayout] position_by none,
    impl_parent: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout] position_by none,
    const_conditions: Table<DefIndex, LazyValue<ty::ConstConditions<'static>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    // FIXME(eddyb) perhaps compute this on the fly if cheap enough?
    coerce_unsized_info: Table<DefIndex, LazyValue<ty::adjustment::CoerceUnsizedInfo>>
        => [Semantic, DecodeLayout] position_by none,
    mir_const_qualif: Table<DefIndex, LazyValue<mir::ConstQualifs>>
        => [Semantic, DecodeLayout] position_by none,
    rendered_const: Table<DefIndex, LazyValue<String>>
        => [Semantic, DecodeLayout] position_by none,
    rendered_precise_capturing_args: Table<DefIndex, LazyArray<PreciseCapturingArgKind<Symbol, Symbol>>>
        => [Semantic, DecodeLayout] position_by none,
    fn_arg_idents: Table<DefIndex, LazyArray<Option<Ident>>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    coroutine_kind: Table<DefIndex, hir::CoroutineKind>
        => [Semantic, DecodeLayout] position_by none,
    coroutine_for_closure: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout] position_by none,
    adt_destructor: Table<DefIndex, LazyValue<ty::Destructor>>
        => [Semantic, DecodeLayout] position_by none,
    adt_async_destructor: Table<DefIndex, LazyValue<ty::AsyncDestructor>>
        => [Semantic, DecodeLayout] position_by none,
    coroutine_by_move_body_def_id: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout] position_by none,
    eval_static_initializer: Table<DefIndex, LazyValue<mir::interpret::ConstAllocation<'static>>>
        => [Semantic, DecodeLayout] position_by none,
    trait_def: Table<DefIndex, LazyValue<ty::TraitDef>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    expn_that_defined: Table<DefIndex, LazyValue<ExpnId>>
        => [Semantic, DecodeLayout] position_by none,
    default_fields: Table<DefIndex, LazyValue<DefId>>
        => [Semantic, DecodeLayout] position_by none,
    params_in_repr: Table<DefIndex, LazyValue<DenseBitSet<u32>>>
        => [Semantic, DecodeLayout] position_by none,
    repr_options: Table<DefIndex, LazyValue<ReprOptions>>
        => [Semantic, DecodeLayout] position_by none,
    // `def_keys` and `def_path_hashes` represent a lazy version of a
    // `DefPathTable`. This allows us to avoid deserializing an entire
    // `DefPathTable` up front, since we may only ever use a few
    // definitions from any given crate.
    def_keys: Table<DefIndex, LazyValue<DefKey>>
        => [Semantic, DecodeLayout] position_by none,
    variant_data: Table<DefIndex, LazyValue<VariantData>>
        => [Semantic, DecodeLayout] position_by none,
    assoc_container: Table<DefIndex, LazyValue<ty::AssocContainer>>
        => [Semantic, DecodeLayout] position_by none,
    macro_definition: Table<DefIndex, LazyValue<ast::DelimArgs>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    deduced_param_attrs: Table<DefIndex, LazyArray<DeducedParamAttrs>>
        => [Semantic, DecodeLayout] position_by none,
    collect_return_position_impl_trait_in_trait_tys: Table<DefIndex, LazyValue<DefIdMap<ty::EarlyBinder<'static, Ty<'static>>>>>
        => [Semantic, DecodeLayout] position_by none,
    doc_link_resolutions: Table<DefIndex, LazyValue<DocLinkResMap>>
        => [Semantic, DecodeLayout] position_by none,
    doc_link_traits_in_scope: Table<DefIndex, LazyArray<DefId>>
        => [Semantic, DecodeLayout] position_by none,
    assumed_wf_types_for_rpitit: Table<DefIndex, LazyArray<(Ty<'static>, Span)>>
        => [Semantic, DecodeLayout] position_by encode_def_ids,
    opaque_ty_origin: Table<DefIndex, LazyValue<hir::OpaqueTyOrigin<DefId>>>
        => [Semantic, DecodeLayout] position_by none,
    anon_const_kind: Table<DefIndex, LazyValue<ty::AnonConstKind>>
        => [Semantic, DecodeLayout] position_by none,
    const_of_item: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::Const<'static>>>>
        => [Semantic, DecodeLayout] position_by none,
    associated_types_for_impl_traits_in_trait_or_impl: Table<DefIndex, LazyValue<DefIdMap<Vec<DefId>>>>
        => [Semantic, DecodeLayout] position_by none,
    args_known_to_outlive_alias_params: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, Vec<(ty::Region<'static>, Vec<ty::GenericArg<'static>>)>>>>
        => [Semantic, DecodeLayout] position_by none,
    mut_restriction: Table<DefIndex, LazyValue<ty::RestrictionKind>>
        => [Semantic, DecodeLayout] position_by none,
- ordinal:
- optional:
    proc_macro_quoted_spans: Table<usize, LazyValue<Span>>
        => [Semantic, DecodeLayout] position_by encode_proc_macros,
}

mod encoder;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct MetadataRecordKey {
    record: PersistedRecord,
    owner: Option<DefPathHash>,
}

impl MetadataRecordKey {
    fn is_selected(self, selection: SelectedDefinitions<'_>) -> bool {
        match self.owner {
            Some(owner) => match selection.state(owner) {
                DefinitionState::Unselected => false,
                DefinitionState::DecodeLayout | DefinitionState::Semantic => true,
            },
            None => true,
        }
    }

    fn is_semantic(self, selection: SelectedDefinitions<'_>) -> bool {
        match self.owner {
            Some(owner) => match selection.state(owner) {
                DefinitionState::Semantic => true,
                DefinitionState::Unselected | DefinitionState::DecodeLayout => false,
            },
            None => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SpanOccurrence {
    record: MetadataRecordKey,
    ordinal: u32,
}

#[derive(Clone, Copy, Decodable, Encodable)]
enum RdrSpanLocation {
    Dummy,
    Position(ExternalSpanSlot),
}

#[derive(Default)]
struct SpanLayout {
    slots: FxIndexSet<SpanOccurrence>,
}

impl SpanLayout {
    fn push(&mut self, occurrence: SpanOccurrence) -> ExternalSpanSlot {
        let (index, inserted) = self.slots.insert_full(occurrence);
        assert!(inserted, "duplicate exported span occurrence {occurrence:?}");
        ExternalSpanSlot::from_usize(index)
    }

    fn metadata_layout(&self) -> MetadataSpanLayout {
        let mut hasher = StableHasher::new();
        for occurrence in &self.slots {
            occurrence.hash(&mut hasher);
        }
        MetadataSpanLayout {
            id: hasher.finish(),
            slot_count: self
                .slots
                .len()
                .try_into()
                .expect("cannot export more than U32_MAX span occurrences"),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MetadataArtifactKind {
    Full,
    RdrMetadata,
    Spans,
    Stub,
}

impl PersistedRecord {
    const fn name(self) -> &'static str {
        match self {
            Self::Artifact(record) => record.name(),
            Self::CrateRoot(field) => field.name(),
            Self::Table(table) => table.name(),
        }
    }
}

#[derive(TyEncodable, TyDecodable)]
struct VariantData {
    idx: VariantIdx,
    discr: ty::VariantDiscr,
    /// If this is unit or tuple-variant/struct, then this is the index of the ctor id.
    ctor: Option<(CtorKind, DefIndex)>,
    is_non_exhaustive: bool,
}

bitflags::bitflags! {
    #[derive(Default)]
    pub struct AttrFlags: u8 {
        const IS_DOC_HIDDEN = 1 << 0;
    }
}

/// A span tag byte encodes a bunch of data, so that we can cut out a few extra bytes from span
/// encodings (which are very common, for example, libcore has ~650,000 unique spans and over 1.1
/// million references to prior-written spans).
///
/// The byte format is split into several parts:
///
/// [ a a a a a c d d ]
///
/// `a` bits represent the span length. We have 5 bits, so we can store lengths up to 30 inline, with
/// an all-1s pattern representing that the length is stored separately.
///
/// `c` represents whether the span context is zero (and then it is not stored as a separate varint)
/// for direct span encodings, and whether the offset is absolute or relative otherwise (zero for
/// absolute).
///
/// d bits represent the kind of span we are storing (local, foreign, partial, indirect).
#[derive(Encodable, Decodable, Copy, Clone)]
struct SpanTag(u8);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SpanKind {
    Local = 0b00,
    Foreign = 0b01,
    Partial = 0b10,
    // Indicates the actual span contents are elsewhere.
    // If this is the kind, then the span context bit represents whether it is a relative or
    // absolute offset.
    Indirect = 0b11,
}

impl SpanTag {
    fn new(kind: SpanKind, context: rustc_span::SyntaxContext, length: usize) -> SpanTag {
        let mut data = 0u8;
        data |= kind as u8;
        if context.is_root() {
            data |= 0b100;
        }
        let all_1s_len = (0xffu8 << 3) >> 3;
        // strictly less than - all 1s pattern is a sentinel for storage being out of band.
        if length < all_1s_len as usize {
            data |= (length as u8) << 3;
        } else {
            data |= all_1s_len << 3;
        }

        SpanTag(data)
    }

    fn indirect(relative: bool, length_bytes: u8) -> SpanTag {
        let mut tag = SpanTag(SpanKind::Indirect as u8);
        if relative {
            tag.0 |= 0b100;
        }
        assert!(length_bytes <= 8);
        tag.0 |= length_bytes << 3;
        tag
    }

    fn kind(self) -> SpanKind {
        let masked = self.0 & 0b11;
        match masked {
            0b00 => SpanKind::Local,
            0b01 => SpanKind::Foreign,
            0b10 => SpanKind::Partial,
            0b11 => SpanKind::Indirect,
            _ => unreachable!(),
        }
    }

    fn is_relative_offset(self) -> bool {
        debug_assert_eq!(self.kind(), SpanKind::Indirect);
        self.0 & 0b100 != 0
    }

    fn context(self) -> Option<rustc_span::SyntaxContext> {
        if self.0 & 0b100 != 0 { Some(rustc_span::SyntaxContext::root()) } else { None }
    }

    fn length(self) -> Option<rustc_span::BytePos> {
        let all_1s_len = (0xffu8 << 3) >> 3;
        let len = self.0 >> 3;
        if len != all_1s_len { Some(rustc_span::BytePos(u32::from(len))) } else { None }
    }
}

// Tags for encoding Symbol's
const SYMBOL_STR: u8 = 0;
const SYMBOL_OFFSET: u8 = 1;
const SYMBOL_PREDEFINED: u8 = 2;

pub fn provide(providers: &mut Providers) {
    encoder::provide(&mut providers.queries);
    decoder::provide(providers);
}

#[cfg(test)]
use rustc_data_structures::owned_slice::slice_owned;
#[cfg(test)]
use rustc_hir::def_id::CRATE_DEF_ID;
#[cfg(test)]
use rustc_middle::metadata::MetadataDefinitionLayout;

#[cfg(test)]
use self::encoder::{MetadataProjectionEncoder, SemanticRecordMembership};

#[cfg(test)]
macro_rules! assert_not_metadata_semantic_value {
    ($ty:ty) => {{
        trait AmbiguousIfSemantic<A> {
            fn assert() {}
        }
        impl<T: ?Sized> AmbiguousIfSemantic<()> for T {}
        impl<T: crate::rmeta::MetadataSemanticValue + ?Sized> AmbiguousIfSemantic<u8> for T {}

        let _ = <$ty as AmbiguousIfSemantic<_>>::assert;
    }};
}

#[test]
fn parses_coarse_metadata_envelope() {
    let bytes = [
        b'r',
        b'u',
        b's',
        b't',
        METADATA_ENVELOPE_VERSION,
        MetadataFormatKind::Coarse as u8,
        0,
        METADATA_VERSION,
    ];

    assert_eq!(parse_metadata_envelope(&bytes).unwrap().format, MetadataFormatKind::Coarse);
    assert_eq!(METADATA_ROOT_POSITION_OFFSET, bytes.len());
    assert_eq!(MetadataFormatKind::Coarse.payload_offset(), bytes.len() + 8);
}

#[test]
fn parses_rdr_metadata_envelope_without_payload() {
    let bytes = [
        b'r',
        b'u',
        b's',
        b't',
        METADATA_ENVELOPE_VERSION,
        MetadataFormatKind::RdrV1 as u8,
        0,
        METADATA_VERSION,
    ];

    assert_eq!(parse_metadata_envelope(&bytes).unwrap().format, MetadataFormatKind::RdrV1);
}

#[test]
fn rejects_truncated_rdr_spans_artifact() {
    let spans = vec![0; SpansArtifactHeader::ENCODED_LEN + MAGIC_END_BYTES.len() - 1];

    assert!(SpansArtifactHeader::parse(24, &spans).is_none());
}

#[test]
fn rejects_unrecognized_rdr_spans_artifact_format() {
    let mut spans = vec![0; SpansArtifactHeader::ENCODED_LEN];
    spans[..SpansArtifactHeader::FORMAT_END].copy_from_slice(b"RSP0");
    spans.extend_from_slice(MAGIC_END_BYTES);

    assert!(SpansArtifactHeader::parse(24, &spans).is_none());
}

#[test]
fn rejects_rdr_spans_artifact_without_end_marker() {
    let spans = SpansArtifactHeader { root_position: NonZero::new(25).unwrap() }.to_bytes();

    assert!(SpansArtifactHeader::parse(24, &spans).is_none());
}

#[test]
fn rejects_out_of_bounds_rdr_spans_artifact_root() {
    let mut spans = SpansArtifactHeader { root_position: NonZero::new(usize::MAX).unwrap() }
        .to_bytes()
        .to_vec();
    spans.extend_from_slice(MAGIC_END_BYTES);

    assert!(SpansArtifactHeader::parse(24, &spans).is_none());
}

#[test]
fn parses_rdr_spans_artifact_header() {
    let mut spans = vec![0; 8];
    spans.extend_from_slice(
        &SpansArtifactHeader { root_position: NonZero::new(28).unwrap() }.to_bytes(),
    );
    spans.extend_from_slice(MAGIC_END_BYTES);

    assert_eq!(SpansArtifactHeader::parse(24, &spans).unwrap().root_position.get(), 28);
}

#[test]
fn rejects_rdr_metadata_with_an_out_of_bounds_metadata_length() {
    let mut primary = MetadataFormatKind::RdrV1.header().to_vec();
    primary.extend_from_slice(&0u64.to_le_bytes());
    primary.extend_from_slice(&u64::MAX.to_le_bytes());
    primary.extend_from_slice(MAGIC_END_BYTES);
    let input = MetadataInput::Rdr(RdrArtifactPair {
        primary: slice_owned(primary, Vec::as_slice),
        spans: SpansArtifactSource::in_memory(slice_owned(Vec::new(), Vec::as_slice)),
    });

    assert!(matches!(LoadedMetadata::new(input), Err(MetadataBlobError::InvalidEncoding)));
}

#[test]
fn rejects_unknown_metadata_format() {
    let bytes = [b'r', b'u', b's', b't', METADATA_ENVELOPE_VERSION, 17, 0, METADATA_VERSION];

    assert_eq!(parse_metadata_envelope(&bytes), Err(MetadataEnvelopeError::UnsupportedFormat(17)));
}

#[test]
fn rejects_unknown_metadata_envelope_version() {
    let bytes = [
        b'r',
        b'u',
        b's',
        b't',
        METADATA_ENVELOPE_VERSION + 1,
        MetadataFormatKind::Coarse as u8,
        0,
        METADATA_VERSION,
    ];

    assert_eq!(
        parse_metadata_envelope(&bytes),
        Err(MetadataEnvelopeError::UnsupportedEnvelopeVersion(METADATA_ENVELOPE_VERSION + 1))
    );
}

#[test]
fn rejects_truncated_metadata_envelope() {
    assert_eq!(parse_metadata_envelope(b"rust"), Err(MetadataEnvelopeError::Truncated));
}

#[test]
fn table_inventory_distinguishes_identity_from_position_data() {
    let def_path_hashes = PersistedRecord::Table(PersistedTable::def_path_hashes).projections();
    assert!(def_path_hashes.contains(PersistedProjection::Semantic));
    assert!(def_path_hashes.contains(PersistedProjection::DecodeLayout));
    assert!(!def_path_hashes.contains(PersistedProjection::Position));

    let def_span = PersistedRecord::Table(PersistedTable::def_span).projections();
    assert!(def_span.contains(PersistedProjection::Semantic));
    assert!(def_span.contains(PersistedProjection::DecodeLayout));
    assert!(def_span.contains(PersistedProjection::Position));
}

#[test]
fn identity_fields_are_explicitly_excluded_from_their_own_projection() {
    let header = PersistedRecord::CrateRoot(PersistedCrateRootField::header).projections();
    assert!(header.contains(PersistedProjection::Semantic));
    assert!(!header.contains(PersistedProjection::DecodeLayout));
    assert!(!header.contains(PersistedProjection::SelfIdentity));

    let decode_layout_id =
        PersistedRecord::CrateRoot(PersistedCrateRootField::metadata_decode_layout_id)
            .projections();
    assert!(!decode_layout_id.contains(PersistedProjection::Semantic));
    assert!(!decode_layout_id.contains(PersistedProjection::DecodeLayout));
    assert!(decode_layout_id.contains(PersistedProjection::SelfIdentity));
}

#[test]
fn artifact_local_values_cannot_enter_the_semantic_projection() {
    assert_not_metadata_semantic_value!(DefIndex);
    assert_not_metadata_semantic_value!(rustc_ast::NodeId);
    assert_not_metadata_semantic_value!(rustc_span::hygiene::ExpnIndex);
    assert_not_metadata_semantic_value!(crate::rmeta::RawDefId);
    assert_not_metadata_semantic_value!(crate::rmeta::LazyValue<()>);
    assert_not_metadata_semantic_value!(crate::rmeta::LazyArray<()>);
    assert_not_metadata_semantic_value!(crate::rmeta::LazyTable<(), ()>);
}

#[test]
fn entering_a_nonsemantic_record_does_not_seed_the_semantic_digest() {
    let owner = DefPathHash(Fingerprint::new(1, 2));
    let key = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_kind),
        owner: Some(owner),
    };
    let (empty_contract, empty_decode_layout) = MetadataProjectionEncoder::new().finish();
    let mut projected = MetadataProjectionEncoder::new();

    projected.enter(key, SemanticRecordMembership::Omitted);
    let (projected_contract, projected_decode_layout) = projected.finish();

    assert_eq!(projected_contract, empty_contract);
    assert_ne!(projected_decode_layout, empty_decode_layout);
}

#[test]
fn wire_bytes_do_not_enter_the_semantic_projection() {
    let key =
        MetadataRecordKey { record: PersistedRecord::Table(PersistedTable::def_span), owner: None };
    let mut original = MetadataProjectionEncoder::new();
    original.enter(key, SemanticRecordMembership::Included);
    original.record_wire_bytes(key, b"slot 1");
    let (original_contract, original_decode_layout) = original.finish();
    let mut moved = MetadataProjectionEncoder::new();
    moved.enter(key, SemanticRecordMembership::Included);
    moved.record_wire_bytes(key, b"slot 2");
    let (moved_contract, moved_decode_layout) = moved.finish();

    assert_eq!(original_contract, moved_contract);
    assert_ne!(original_decode_layout, moved_decode_layout);
}

#[test]
fn spans_wire_bytes_do_not_enter_metadata_identity() {
    let key = MetadataRecordKey {
        record: PersistedRecord::Artifact(PersistedArtifactRecord::spans),
        owner: None,
    };
    let mut original = MetadataProjectionEncoder::new();
    original.enter(key, SemanticRecordMembership::Included);
    original.record_wire_bytes(key, b"1:1");
    let (original_contract, original_decode_layout) = original.finish();
    let mut moved = MetadataProjectionEncoder::new();
    moved.enter(key, SemanticRecordMembership::Included);
    moved.record_wire_bytes(key, b"20:4");
    let (moved_contract, moved_decode_layout) = moved.finish();

    assert_eq!(original_contract, moved_contract);
    assert_eq!(original_decode_layout, moved_decode_layout);
}

#[test]
fn emitted_identity_values_do_not_hash_themselves() {
    let header = MetadataRecordKey {
        record: PersistedRecord::CrateRoot(PersistedCrateRootField::header),
        owner: None,
    };
    let decode_layout = MetadataRecordKey {
        record: PersistedRecord::CrateRoot(PersistedCrateRootField::metadata_decode_layout_id),
        owner: None,
    };
    let mut projected = MetadataProjectionEncoder::new();
    projected.enter(header, SemanticRecordMembership::Included);
    projected.record_semantic(header, Fingerprint::new(1, 2));
    projected.record_wire_bytes(header, b"placeholder contract");
    projected.enter(decode_layout, SemanticRecordMembership::Included);
    projected.record_wire_bytes(decode_layout, b"placeholder layout");
    let (projected_contract, projected_decode_layout) = projected.finish();
    let mut emitted = MetadataProjectionEncoder::new();
    emitted.enter(header, SemanticRecordMembership::Included);
    emitted.record_semantic(header, Fingerprint::new(1, 2));
    emitted.record_wire_bytes(header, b"converged contract");
    emitted.enter(decode_layout, SemanticRecordMembership::Included);
    emitted.record_wire_bytes(decode_layout, b"converged layout");
    let (emitted_contract, emitted_decode_layout) = emitted.finish();

    assert_eq!(projected_contract, emitted_contract);
    assert_eq!(projected_decode_layout, emitted_decode_layout);
}

#[test]
fn projection_digests_include_empty_records() {
    let (empty_contract, empty_decode_layout) = MetadataProjectionEncoder::new().finish();
    let mut declared = MetadataProjectionEncoder::new();
    declared.enter(
        MetadataRecordKey {
            record: PersistedRecord::Table(PersistedTable::attributes),
            owner: None,
        },
        SemanticRecordMembership::Included,
    );
    let (declared_contract, declared_decode_layout) = declared.finish();

    assert_ne!(empty_contract, declared_contract);
    assert_ne!(empty_decode_layout, declared_decode_layout);
}

#[test]
fn span_layout_uses_exported_occurrences_not_coordinates() {
    let owner = DefPathHash(Fingerprint::new(1, 1));
    let record = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_span),
        owner: Some(owner),
    };
    let mut layout = SpanLayout::default();
    layout.push(SpanOccurrence { record, ordinal: 0 });
    layout.push(SpanOccurrence { record, ordinal: 1 });

    assert_ne!(
        layout.slots.get_index_of(&SpanOccurrence { record, ordinal: 0 }),
        layout.slots.get_index_of(&SpanOccurrence { record, ordinal: 1 })
    );
}

#[test]
fn private_occurrences_do_not_change_exported_slots() {
    let exported_owner = DefPathHash(Fingerprint::new(1, 1));
    let private_owner = DefPathHash(Fingerprint::new(1, 2));
    let exported_def_id = LocalDefId { local_def_index: DefIndex::from_u32(1) };
    let exported = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_span),
        owner: Some(exported_owner),
    };
    let private = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_span),
        owner: Some(private_owner),
    };
    let layout = MetadataDefinitionLayout::from_artifact(
        [
            (CRATE_DEF_ID, DefPathHash(Fingerprint::new(1, 0)), None),
            (exported_def_id, exported_owner, Some(CRATE_DEF_ID)),
        ]
        .into_iter(),
    )
    .unwrap();
    let selection = layout.selection();
    let exported_occurrence = SpanOccurrence { record: exported, ordinal: 0 };
    let mut first = SpanLayout::default();
    first.push(exported_occurrence);
    let mut second = SpanLayout::default();
    for occurrence in [SpanOccurrence { record: private, ordinal: 0 }, exported_occurrence] {
        if occurrence.record.is_selected(selection) {
            second.push(occurrence);
        }
    }

    assert_eq!(
        first.slots.get_index_of(&SpanOccurrence { record: exported, ordinal: 0 }),
        second.slots.get_index_of(&SpanOccurrence { record: exported, ordinal: 0 })
    );
    assert_eq!(first.metadata_layout(), second.metadata_layout());
}

#[test]
fn exported_occurrence_order_changes_layout_identity() {
    let left_owner = DefPathHash(Fingerprint::new(1, 1));
    let right_owner = DefPathHash(Fingerprint::new(1, 2));
    let left = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_span),
        owner: Some(left_owner),
    };
    let right = MetadataRecordKey {
        record: PersistedRecord::Table(PersistedTable::def_ident_span),
        owner: Some(right_owner),
    };
    let mut forward = SpanLayout::default();
    forward.push(SpanOccurrence { record: left, ordinal: 0 });
    forward.push(SpanOccurrence { record: right, ordinal: 0 });
    let mut reverse = SpanLayout::default();
    reverse.push(SpanOccurrence { record: right, ordinal: 0 });
    reverse.push(SpanOccurrence { record: left, ordinal: 0 });

    assert_ne!(forward.metadata_layout(), reverse.metadata_layout());
}
