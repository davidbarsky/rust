use std::hash::Hash;
use std::marker::PhantomData;
use std::num::NonZero;

use decoder::LazyDecoder;
pub(crate) use decoder::{CrateMetadata, CrateNumMap, MetadataBlob, TargetModifiers};
use def_path_hash_map::DefPathHashMapRef;
use encoder::EncodeContext;
pub use encoder::{EncodedMetadata, encode_metadata, rendered_const};
pub(crate) use parameterized::ParameterizedOverTcx;
use rustc_abi::{FieldIdx, ReprOptions, VariantIdx};
use rustc_ast as ast;
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
    AmbigModChild, DefinitionState, MetadataSpanLayout, ModChild, SelectedDefinitions,
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

/// Metadata header which includes `METADATA_VERSION`.
///
/// This header is followed by the length of the compressed data, then
/// the position of the `CrateRoot`, which is encoded as a 64-bit little-endian
/// unsigned integer, and further followed by the rustc version string.
pub const METADATA_HEADER: &[u8] = &[b'r', b'u', b's', b't', 0, 0, 0, METADATA_VERSION];

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
    pub(crate) hash: Svh,
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
    hash: Svh,
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
                    => [$($projection:ident),+],
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
                        => [$($definition_defaulted_projection:ident),+],
                )+
            - optional:
                $(
                    $definition_optional:ident: Table<DefIndex, $definition_optional_value:ty>
                        => [$($definition_optional_projection:ident),+],
                )+
        - ordinal:
            - optional:
                $(
                    $ordinal_optional:ident: Table<$ordinal_index:ty, $ordinal_optional_value:ty>
                        => [$($ordinal_optional_projection:ident),+],
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
        - root:
            $(#[$root_attr:meta])*
            $root_visibility:vis struct $root:ident {
                $(
                    $(#[$field_attr:meta])*
                    $field_visibility:vis $field:ident: $field_type:ty
                        => [$($field_projection:ident),+],
                )+
            }
        - tables:
            - definition:
                - defaulted:
                    $(
                        $definition_defaulted:ident:
                            Table<DefIndex, $definition_defaulted_value:ty>
                            => [$($definition_defaulted_projection:ident),+],
                    )+
                - optional:
                    $(
                        $definition_optional:ident:
                            Table<DefIndex, $definition_optional_value:ty>
                            => [$($definition_optional_projection:ident),+],
                    )+
            - ordinal:
                - optional:
                    $(
                        $ordinal_optional:ident:
                            Table<$ordinal_index:ty, $ordinal_optional_value:ty>
                            => [$($ordinal_optional_projection:ident),+],
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
                        => [$($field_projection),+],
                )+
            }
        }

        define_tables! {
            - definition:
                - defaulted:
                    $(
                        $definition_defaulted: Table<DefIndex, $definition_defaulted_value>
                            => [$($definition_defaulted_projection),+],
                    )+
                - optional:
                    $(
                        $definition_optional: Table<DefIndex, $definition_optional_value>
                            => [$($definition_optional_projection),+],
                    )+
            - ordinal:
                - optional:
                    $(
                        $ordinal_optional: Table<$ordinal_index, $ordinal_optional_value>
                            => [$($ordinal_optional_projection),+],
                    )+
        }

        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub(crate) enum PersistedRecord {
            Artifact(PersistedArtifactRecord),
            CrateRoot(PersistedCrateRootField),
            Table(PersistedTable),
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
                        Self::CrateRoot(PersistedCrateRootField::$field) =>
                            persisted_projections!($($field_projection),+),
                    )+
                    $(
                        Self::Table(PersistedTable::$definition_defaulted) =>
                            persisted_projections!($($definition_defaulted_projection),+),
                    )+
                    $(
                        Self::Table(PersistedTable::$definition_optional) =>
                            persisted_projections!($($definition_optional_projection),+),
                    )+
                    $(
                        Self::Table(PersistedTable::$ordinal_optional) =>
                            persisted_projections!($($ordinal_optional_projection),+),
                    )+
                }
            }
        }

        impl MetadataArtifactKind {
            fn contains(self, record: PersistedRecord) -> bool {
                match record {
                    PersistedRecord::Artifact(record) => record.artifacts().contains(&self),
                    PersistedRecord::CrateRoot(_) | PersistedRecord::Table(_) => match self {
                        Self::Full => true,
                        Self::Stub => false,
                    },
                }
            }
        }
    }
}

define_metadata_schema! {
- artifacts:
    metadata_header in [Full, Stub] => [DecodeLayout],
    root_position in [Full, Stub] => [DecodeLayout],
    rustc_version in [Full, Stub] => [DecodeLayout],
    stub_crate_header in [Stub] => [Semantic, DecodeLayout],
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
    header: CrateHeader => [Semantic],

    extra_filename: String => [DecodeLayout],
    stable_crate_id: StableCrateId => [Semantic, DecodeLayout],
    required_panic_strategy: Option<PanicStrategy> => [Semantic],
    panic_in_drop_strategy: PanicStrategy => [Semantic],
    edition: Edition => [Semantic],
    has_global_allocator: bool => [Semantic],
    has_alloc_error_handler: bool => [Semantic],
    has_panic_handler: bool => [Semantic],
    has_default_lib_allocator: bool => [Semantic],
    externally_implementable_items: LazyArray<EiiMapEncodedKeyValue>
        => [Semantic, DecodeLayout],

    crate_deps: LazyArray<CrateDep> => [Semantic, DecodeLayout],
    dylib_dependency_formats: LazyArray<Option<LinkagePreference>> => [Semantic, DecodeLayout],
    lib_features: LazyArray<(Symbol, FeatureStability)> => [Semantic, DecodeLayout],
    stability_implications: LazyArray<(Symbol, Symbol)> => [Semantic, DecodeLayout],
    lang_items: LazyArray<(DefIndex, LangItem)> => [Semantic, DecodeLayout],
    lang_items_missing: LazyArray<LangItem> => [Semantic, DecodeLayout],
    stripped_cfg_items: LazyArray<StrippedCfgItem<DefIndex>>
        => [Semantic, DecodeLayout],
    diagnostic_items: LazyArray<(Symbol, DefIndex)> => [Semantic, DecodeLayout],
    canonical_symbols: LazyArray<(Symbol, DefIndex)> => [Semantic, DecodeLayout],
    native_libraries: LazyArray<NativeLib> => [Semantic, DecodeLayout],
    foreign_modules: LazyArray<ForeignModule> => [Semantic, DecodeLayout],
    traits: LazyArray<DefIndex> => [Semantic, DecodeLayout],
    impls: LazyArray<TraitImpls> => [Semantic, DecodeLayout],
    incoherent_impls: LazyArray<IncoherentImpls> => [Semantic, DecodeLayout],
    interpret_alloc_index: LazyArray<u64> => [DecodeLayout],
    proc_macro_data: Option<ProcMacroData> => [DecodeLayout],

    tables: LazyTables => [DecodeLayout],
    debugger_visualizers: LazyArray<DebuggerVisualizerFile> => [Semantic, DecodeLayout],

    exportable_items: LazyArray<DefIndex> => [Semantic, DecodeLayout],
    stable_order_of_exportable_impls: LazyArray<(DefIndex, usize)> => [Semantic, DecodeLayout],
    exported_non_generic_symbols: LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>
        => [Semantic, DecodeLayout],
    exported_generic_symbols: LazyArray<(ExportedSymbol<'static>, SymbolExportInfo)>
        => [Semantic, DecodeLayout],

    syntax_contexts: SyntaxContextTable => [Semantic, DecodeLayout],
    expn_data: ExpnDataTable => [Semantic, DecodeLayout],
    expn_hashes: ExpnHashTable => [Semantic, DecodeLayout],

    def_path_hash_map: LazyValue<DefPathHashMapRef<'static>> => [DecodeLayout],

    source_map: LazyTable<u32, Option<LazyValue<rustc_span::SourceFile>>> => [DecodeLayout],
    target_modifiers: LazyArray<TargetModifier> => [Semantic, DecodeLayout],
    denied_partial_mitigations: LazyArray<DeniedPartialMitigation> => [Semantic, DecodeLayout],

    compiler_builtins: bool => [Semantic],
    needs_allocator: bool => [Semantic],
    needs_panic_runtime: bool => [Semantic],
    no_builtins: bool => [Semantic],
    panic_runtime: bool => [Semantic],
    profiler_runtime: bool => [Semantic],
    symbol_mangling_version: SymbolManglingVersion => [Semantic],

    specialization_enabled_in: bool => [Semantic],
}
- tables:
- definition:
- defaulted:
    intrinsic: Table<DefIndex, Option<LazyValue<ty::IntrinsicDef>>>
        => [Semantic, DecodeLayout],
    is_macro_rules: Table<DefIndex, bool>
        => [Semantic, DecodeLayout],
    type_alias_is_checked: Table<DefIndex, bool>
        => [Semantic, DecodeLayout],
    attr_flags: Table<DefIndex, AttrFlags>
        => [Semantic, DecodeLayout],
    // The u64 is the crate-local part of the DefPathHash. All hashes in this crate have the same
    // StableCrateId, so we omit encoding those into the table.
    //
    // Note also that this table is fully populated (no gaps) as every DefIndex should have a
    // corresponding DefPathHash.
    def_path_hashes: Table<DefIndex, u64>
        => [Semantic, DecodeLayout],
    explicit_item_bounds: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout],
    explicit_item_self_bounds: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout],
    inferred_outlives_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout],
    explicit_super_clauses_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout],
    explicit_implied_clauses_of: Table<DefIndex, LazyArray<(ty::Clause<'static>, Span)>>
        => [Semantic, DecodeLayout],
    explicit_implied_const_bounds: Table<DefIndex, LazyArray<(ty::PolyTraitRef<'static>, Span)>>
        => [Semantic, DecodeLayout],
    inherent_impls: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout],
    opt_rpitit_info: Table<DefIndex, Option<LazyValue<ty::ImplTraitInTraitData>>>
        => [Semantic, DecodeLayout],
    // Reexported names are not associated with individual `DefId`s,
    // e.g. a glob import can introduce a lot of names, all with the same `DefId`.
    // That's why the encoded list needs to contain `ModChild` structures describing all the names
    // individually instead of `DefId`s.
    module_children_reexports: Table<DefIndex, LazyArray<ModChild>>
        => [Semantic, DecodeLayout],
    ambig_module_children: Table<DefIndex, LazyArray<AmbigModChild>>
        => [Semantic, DecodeLayout],
    cross_crate_inlinable: Table<DefIndex, bool>
        => [Semantic, DecodeLayout],
    asyncness: Table<DefIndex, ty::Asyncness>
        => [Semantic, DecodeLayout],
    constness: Table<DefIndex, hir::Constness>
        => [Semantic, DecodeLayout],
    safety: Table<DefIndex, hir::Safety>
        => [Semantic, DecodeLayout],
    defaultness: Table<DefIndex, hir::Defaultness>
        => [Semantic, DecodeLayout],
    impl_is_fully_generic_for_reflection: Table<DefIndex, bool>
        => [Semantic, DecodeLayout],

- optional:
    attributes: Table<DefIndex, LazyArray<hir::Attribute>>
        => [Semantic, DecodeLayout],
    // For non-reexported names in a module every name is associated with a separate `DefId`,
    // so we can take their names, visibilities etc from other encoded tables.
    module_children_non_reexports: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout],
    associated_item_or_field_def_ids: Table<DefIndex, LazyArray<DefIndex>>
        => [Semantic, DecodeLayout],
    def_kind: Table<DefIndex, DefKind>
        => [Semantic, DecodeLayout],
    visibility: Table<DefIndex, LazyValue<ty::Visibility<DefIndex>>>
        => [Semantic, DecodeLayout],
    def_span: Table<DefIndex, LazyValue<Span>>
        => [Semantic, DecodeLayout],
    def_ident_span: Table<DefIndex, LazyValue<Span>>
        => [Semantic, DecodeLayout],
    lookup_stability: Table<DefIndex, LazyValue<hir::Stability>>
        => [Semantic, DecodeLayout],
    lookup_const_stability: Table<DefIndex, LazyValue<hir::ConstStability>>
        => [Semantic, DecodeLayout],
    lookup_default_body_stability: Table<DefIndex, LazyValue<hir::DefaultBodyStability>>
        => [Semantic, DecodeLayout],
    lookup_deprecation_entry: Table<DefIndex, LazyValue<attrs::Deprecation>>
        => [Semantic, DecodeLayout],
    explicit_clauses_of: Table<DefIndex, LazyValue<ty::GenericClauses<'static>>>
        => [Semantic, DecodeLayout],
    generics_of: Table<DefIndex, LazyValue<ty::Generics>>
        => [Semantic, DecodeLayout],
    type_of: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, Ty<'static>>>>
        => [Semantic, DecodeLayout],
    variances_of: Table<DefIndex, LazyArray<ty::Variance>>
        => [Semantic, DecodeLayout],
    fn_sig: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::PolyFnSig<'static>>>>
        => [Semantic, DecodeLayout],
    codegen_fn_attrs: Table<DefIndex, LazyValue<CodegenFnAttrs>>
        => [Semantic, DecodeLayout],
    impl_trait_header: Table<DefIndex, LazyValue<ty::ImplTraitHeader<'static>>>
        => [Semantic, DecodeLayout],
    const_param_default: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, rustc_middle::ty::Const<'static>>>>
        => [Semantic, DecodeLayout],
    object_lifetime_default: Table<DefIndex, LazyValue<ObjectLifetimeDefault>>
        => [Semantic, DecodeLayout],
    optimized_mir: Table<DefIndex, LazyValue<mir::Body<'static>>>
        => [Semantic, DecodeLayout],
    mir_for_ctfe: Table<DefIndex, LazyValue<mir::Body<'static>>>
        => [Semantic, DecodeLayout],
    trivial_const: Table<DefIndex, LazyValue<(ConstValue, Ty<'static>)>>
        => [Semantic, DecodeLayout],
    closure_saved_names_of_captured_variables: Table<DefIndex, LazyValue<IndexVec<FieldIdx, Symbol>>>
        => [Semantic, DecodeLayout],
    mir_coroutine_witnesses: Table<DefIndex, LazyValue<mir::CoroutineLayout<'static>>>
        => [Semantic, DecodeLayout],
    promoted_mir: Table<DefIndex, LazyValue<IndexVec<mir::Promoted, mir::Body<'static>>>>
        => [Semantic, DecodeLayout],
    thir_abstract_const: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::Const<'static>>>>
        => [Semantic, DecodeLayout],
    impl_parent: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout],
    const_conditions: Table<DefIndex, LazyValue<ty::ConstConditions<'static>>>
        => [Semantic, DecodeLayout],
    // FIXME(eddyb) perhaps compute this on the fly if cheap enough?
    coerce_unsized_info: Table<DefIndex, LazyValue<ty::adjustment::CoerceUnsizedInfo>>
        => [Semantic, DecodeLayout],
    mir_const_qualif: Table<DefIndex, LazyValue<mir::ConstQualifs>>
        => [Semantic, DecodeLayout],
    rendered_const: Table<DefIndex, LazyValue<String>>
        => [Semantic, DecodeLayout],
    rendered_precise_capturing_args: Table<DefIndex, LazyArray<PreciseCapturingArgKind<Symbol, Symbol>>>
        => [Semantic, DecodeLayout],
    fn_arg_idents: Table<DefIndex, LazyArray<Option<Ident>>>
        => [Semantic, DecodeLayout],
    coroutine_kind: Table<DefIndex, hir::CoroutineKind>
        => [Semantic, DecodeLayout],
    coroutine_for_closure: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout],
    adt_destructor: Table<DefIndex, LazyValue<ty::Destructor>>
        => [Semantic, DecodeLayout],
    adt_async_destructor: Table<DefIndex, LazyValue<ty::AsyncDestructor>>
        => [Semantic, DecodeLayout],
    coroutine_by_move_body_def_id: Table<DefIndex, RawDefId>
        => [Semantic, DecodeLayout],
    eval_static_initializer: Table<DefIndex, LazyValue<mir::interpret::ConstAllocation<'static>>>
        => [Semantic, DecodeLayout],
    trait_def: Table<DefIndex, LazyValue<ty::TraitDef>>
        => [Semantic, DecodeLayout],
    expn_that_defined: Table<DefIndex, LazyValue<ExpnId>>
        => [Semantic, DecodeLayout],
    default_fields: Table<DefIndex, LazyValue<DefId>>
        => [Semantic, DecodeLayout],
    params_in_repr: Table<DefIndex, LazyValue<DenseBitSet<u32>>>
        => [Semantic, DecodeLayout],
    repr_options: Table<DefIndex, LazyValue<ReprOptions>>
        => [Semantic, DecodeLayout],
    // `def_keys` and `def_path_hashes` represent a lazy version of a
    // `DefPathTable`. This allows us to avoid deserializing an entire
    // `DefPathTable` up front, since we may only ever use a few
    // definitions from any given crate.
    def_keys: Table<DefIndex, LazyValue<DefKey>>
        => [Semantic, DecodeLayout],
    variant_data: Table<DefIndex, LazyValue<VariantData>>
        => [Semantic, DecodeLayout],
    assoc_container: Table<DefIndex, LazyValue<ty::AssocContainer>>
        => [Semantic, DecodeLayout],
    macro_definition: Table<DefIndex, LazyValue<ast::DelimArgs>>
        => [Semantic, DecodeLayout],
    deduced_param_attrs: Table<DefIndex, LazyArray<DeducedParamAttrs>>
        => [Semantic, DecodeLayout],
    collect_return_position_impl_trait_in_trait_tys: Table<DefIndex, LazyValue<DefIdMap<ty::EarlyBinder<'static, Ty<'static>>>>>
        => [Semantic, DecodeLayout],
    doc_link_resolutions: Table<DefIndex, LazyValue<DocLinkResMap>>
        => [Semantic, DecodeLayout],
    doc_link_traits_in_scope: Table<DefIndex, LazyArray<DefId>>
        => [Semantic, DecodeLayout],
    assumed_wf_types_for_rpitit: Table<DefIndex, LazyArray<(Ty<'static>, Span)>>
        => [Semantic, DecodeLayout],
    opaque_ty_origin: Table<DefIndex, LazyValue<hir::OpaqueTyOrigin<DefId>>>
        => [Semantic, DecodeLayout],
    anon_const_kind: Table<DefIndex, LazyValue<ty::AnonConstKind>>
        => [Semantic, DecodeLayout],
    const_of_item: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, ty::Const<'static>>>>
        => [Semantic, DecodeLayout],
    associated_types_for_impl_traits_in_trait_or_impl: Table<DefIndex, LazyValue<DefIdMap<Vec<DefId>>>>
        => [Semantic, DecodeLayout],
    args_known_to_outlive_alias_params: Table<DefIndex, LazyValue<ty::EarlyBinder<'static, Vec<(ty::Region<'static>, Vec<ty::GenericArg<'static>>)>>>>
        => [Semantic, DecodeLayout],
    mut_restriction: Table<DefIndex, LazyValue<ty::RestrictionKind>>
        => [Semantic, DecodeLayout],
- ordinal:
- optional:
    proc_macro_quoted_spans: Table<usize, LazyValue<Span>>
        => [Semantic, DecodeLayout],
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
use rustc_data_structures::fingerprint::Fingerprint;
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
