//! Machinery for hygienic macros.
//!
//! Inspired by Matthew Flatt et al., “Macros That Work Together: Compile-Time Bindings, Partial
//! Expansion, and Definition Contexts,” *Journal of Functional Programming* 22, no. 2
//! (March 1, 2012): 181–216, <https://doi.org/10.1017/S0956796812000093>.

// Hygiene data is stored in a global variable and accessed via TLS, which
// means that accesses are somewhat expensive. (`HygieneData::with`
// encapsulates a single access.) Therefore, on hot code paths it is worth
// ensuring that multiple HygieneData accesses are combined into a single
// `HygieneData::with`.
//
// This explains why `HygieneData`, `SyntaxContext` and `ExpnId` have interfaces
// with a certain amount of redundancy in them. For example,
// `SyntaxContext::outer_expn_data` combines `SyntaxContext::outer` and
// `ExpnId::expn_data` so that two `HygieneData` accesses can be performed within
// a single `HygieneData::with` call.
//
// It also explains why many functions appear in `HygieneData` and again in
// `SyntaxContext` or `ExpnId`. For example, `HygieneData::outer` and
// `SyntaxContext::outer` do the same thing, but the former is for use within a
// `HygieneData::with` call while the latter is for use outside such a call.
// When modifying this file it is important to understand this distinction,
// because getting it wrong can lead to nested `HygieneData::with` calls that
// trigger runtime aborts. (Fortunately these are obvious and easy to fix.)

use std::collections::VecDeque;
use std::hash::Hash;
use std::sync::Arc;
use std::{fmt, iter, mem};

use rustc_data_structures::fingerprint::Fingerprint;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_data_structures::stable_hash::{
    RawDefId, RawDefPathHash, RawExpnId, RawSpan, SpanHashMode, StableCompare, StableHash,
    StableHashControls, StableHashCtxt, StableHasher, ToStableHashKey,
};
use rustc_data_structures::sync::Lock;
use rustc_data_structures::unhash::UnhashMap;
use rustc_data_structures::unord::{ExtendUnord, UnordMap, UnordSet};
use rustc_hashes::Hash64;
use rustc_index::IndexVec;
use rustc_macros::{Decodable, Encodable, StableHash};
use rustc_serialize::{Decodable, Decoder, Encodable, Encoder};
use tracing::{debug, trace};

use crate::def_id::{CRATE_DEF_ID, CrateNum, DefId, LOCAL_CRATE, ModId, StableCrateId};
use crate::edition::Edition;
use crate::source_map::SourceMap;
use crate::symbol::{Symbol, kw, sym};
use crate::{DUMMY_SP, Span, SpanDecoder, SpanEncoder, with_session_globals};

/// A `SyntaxContext` represents a chain of pairs `(ExpnId, Transparency)` named "marks".
///
/// See <https://rustc-dev-guide.rust-lang.org/macro-expansion.html> for more explanation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SyntaxContext(u32);

// To ensure correctness of incremental compilation,
// `SyntaxContext` must not implement `Ord` or `PartialOrd`.
// See https://github.com/rust-lang/rust/issues/90317.
impl !Ord for SyntaxContext {}
impl !PartialOrd for SyntaxContext {}

/// If this part of two syntax contexts is equal, then the whole syntax contexts should be equal.
/// The other fields are only for caching.
pub type SyntaxContextKey = (SyntaxContext, ExpnId, Transparency);

#[derive(Clone, Copy, Debug)]
struct SyntaxContextData {
    /// The last macro expansion in the chain.
    /// (Here we say the most deeply nested macro expansion is the "outermost" expansion.)
    outer_expn: ExpnId,
    /// Transparency of the last macro expansion
    outer_transparency: Transparency,
    parent: SyntaxContext,
    /// This context, but with all transparent and semi-opaque expansions filtered away.
    opaque: SyntaxContext,
    /// This context, but with all transparent expansions filtered away.
    opaque_and_semiopaque: SyntaxContext,
    /// Name of the crate to which `$crate` with this context would resolve.
    dollar_crate_name: Symbol,
}

impl SyntaxContextData {
    fn root() -> SyntaxContextData {
        SyntaxContextData {
            outer_expn: ExpnId::root(),
            outer_transparency: Transparency::Opaque,
            parent: SyntaxContext::root(),
            opaque: SyntaxContext::root(),
            opaque_and_semiopaque: SyntaxContext::root(),
            dollar_crate_name: kw::DollarCrate,
        }
    }

    fn key(&self) -> SyntaxContextKey {
        (self.parent, self.outer_expn, self.outer_transparency)
    }
}

rustc_index::newtype_index! {
    /// A unique ID associated with a macro invocation and expansion.
    #[orderable]
    pub struct ExpnIndex {}
}

/// A unique ID associated with a macro invocation and expansion.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExpnId {
    pub krate: CrateNum,
    pub local_id: ExpnIndex,
}

impl fmt::Debug for ExpnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Generate crate_::{{expn_}}.
        write!(f, "{:?}::{{{{expn{}}}}}", self.krate, self.local_id.as_u32())
    }
}

rustc_index::newtype_index! {
    /// A unique ID associated with a macro invocation and expansion.
    #[debug_format = "expn{}"]
    pub struct LocalExpnId {}
}

// To ensure correctness of incremental compilation,
// `LocalExpnId` must not implement `Ord` or `PartialOrd`.
// See https://github.com/rust-lang/rust/issues/90317.
impl !Ord for LocalExpnId {}
impl !PartialOrd for LocalExpnId {}

/// A unique hash value associated to an expansion.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Encodable, Decodable, StableHash)]
pub struct ExpnHash(Fingerprint);

impl ExpnHash {
    /// Returns the [StableCrateId] identifying the crate this [ExpnHash]
    /// originates from.
    #[inline]
    pub fn stable_crate_id(self) -> StableCrateId {
        StableCrateId(self.0.split().0)
    }

    /// Returns the crate-local part of the [ExpnHash].
    ///
    /// Used for assertions.
    #[inline]
    pub fn local_hash(self) -> Hash64 {
        self.0.split().1
    }

    #[inline]
    pub fn is_root(self) -> bool {
        self.0 == Fingerprint::ZERO
    }

    /// Builds a new [ExpnHash] with the given [StableCrateId] and
    /// `local_hash`, where `local_hash` must be unique within its crate.
    fn new(stable_crate_id: StableCrateId, local_hash: Hash64) -> ExpnHash {
        ExpnHash(Fingerprint::new(stable_crate_id.0, local_hash))
    }
}

impl StableCompare for ExpnHash {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    fn stable_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// A property of a macro expansion that determines how identifiers
/// produced by that expansion are resolved.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Hash, Debug, Encodable, Decodable)]
#[derive(StableHash)]
pub enum Transparency {
    /// Identifier produced by a transparent expansion is always resolved at call-site.
    /// Call-site spans in procedural macros, hygiene opt-out in `macro` should use this.
    Transparent,
    /// Identifier produced by a semi-opaque expansion may be resolved
    /// either at call-site or at definition-site.
    /// If it's a local variable, label or `$crate` then it's resolved at def-site.
    /// Otherwise it's resolved at call-site.
    /// `macro_rules` macros behave like this, built-in macros currently behave like this too,
    /// but that's an implementation detail.
    SemiOpaque,
    /// Identifier produced by an opaque expansion is always resolved at definition-site.
    /// Def-site spans in procedural macros, identifiers from `macro` by default use this.
    Opaque,
}

impl Transparency {
    pub fn fallback(macro_rules: bool) -> Self {
        if macro_rules { Transparency::SemiOpaque } else { Transparency::Opaque }
    }
}

impl LocalExpnId {
    /// The ID of the theoretical expansion that generates freshly parsed, unexpanded AST.
    pub const ROOT: LocalExpnId = LocalExpnId::ZERO;

    #[inline]
    fn from_raw(idx: ExpnIndex) -> LocalExpnId {
        LocalExpnId::from_u32(idx.as_u32())
    }

    #[inline]
    pub fn as_raw(self) -> ExpnIndex {
        ExpnIndex::from_u32(self.as_u32())
    }

    pub fn fresh_empty() -> LocalExpnId {
        HygieneData::with(|data| {
            let expn_id = data.local_expn_data.push(None);
            let _eid = data.local_expn_hashes.push(ExpnHash(Fingerprint::ZERO));
            debug_assert_eq!(expn_id, _eid);
            expn_id
        })
    }

    pub fn fresh(mut expn_data: ExpnData, hcx: impl StableHashCtxt) -> LocalExpnId {
        debug_assert_eq!(expn_data.parent.krate, LOCAL_CRATE);
        let expn_hash = update_disambiguator(&mut expn_data, hcx);
        HygieneData::with(|data| {
            let expn_id = data.local_expn_data.push(Some(expn_data));
            let _eid = data.local_expn_hashes.push(expn_hash);
            debug_assert_eq!(expn_id, _eid);
            let _old_id = data.expn_hash_to_expn_id.insert(expn_hash, expn_id.to_expn_id());
            debug_assert!(_old_id.is_none());
            expn_id
        })
    }

    #[inline]
    pub fn expn_data(self) -> ExpnData {
        HygieneData::with(|data| data.local_expn_data(self).clone())
    }

    #[inline]
    pub fn to_expn_id(self) -> ExpnId {
        ExpnId { krate: LOCAL_CRATE, local_id: self.as_raw() }
    }

    #[inline]
    pub fn set_expn_data(self, mut expn_data: ExpnData, hcx: impl StableHashCtxt) {
        debug_assert_eq!(expn_data.parent.krate, LOCAL_CRATE);
        let expn_hash = update_disambiguator(&mut expn_data, hcx);
        HygieneData::with(|data| {
            let old_expn_data = &mut data.local_expn_data[self];
            assert!(old_expn_data.is_none(), "expansion data is reset for an expansion ID");
            *old_expn_data = Some(expn_data);
            debug_assert_eq!(data.local_expn_hashes[self].0, Fingerprint::ZERO);
            data.local_expn_hashes[self] = expn_hash;
            let _old_id = data.expn_hash_to_expn_id.insert(expn_hash, self.to_expn_id());
            debug_assert!(_old_id.is_none());
        });
    }

    #[inline]
    pub fn is_descendant_of(self, ancestor: LocalExpnId) -> bool {
        self.to_expn_id().is_descendant_of(ancestor.to_expn_id())
    }

    /// Returns span for the macro which originally caused this expansion to happen.
    ///
    /// Stops backtracing at include! boundary.
    #[inline]
    pub fn expansion_cause(self) -> Option<Span> {
        self.to_expn_id().expansion_cause()
    }
}

impl ExpnId {
    /// The ID of the theoretical expansion that generates freshly parsed, unexpanded AST.
    /// Invariant: we do not create any ExpnId with local_id == 0 and krate != 0.
    pub const fn root() -> ExpnId {
        ExpnId { krate: LOCAL_CRATE, local_id: ExpnIndex::ZERO }
    }

    #[inline]
    pub fn expn_hash(self) -> ExpnHash {
        HygieneData::with(|data| data.expn_hash(self))
    }

    #[inline]
    pub fn from_hash(hash: ExpnHash) -> Option<ExpnId> {
        HygieneData::with(|data| data.expn_hash_to_expn_id.get(&hash).copied())
    }

    #[inline]
    pub fn as_local(self) -> Option<LocalExpnId> {
        if self.krate == LOCAL_CRATE { Some(LocalExpnId::from_raw(self.local_id)) } else { None }
    }

    #[inline]
    #[track_caller]
    pub fn expect_local(self) -> LocalExpnId {
        self.as_local().unwrap()
    }

    #[inline]
    pub fn expn_data(self) -> ExpnData {
        HygieneData::with(|data| data.expn_data(self).clone())
    }

    #[inline]
    pub fn is_descendant_of(self, ancestor: ExpnId) -> bool {
        // a few "fast path" cases to avoid locking HygieneData
        if ancestor == ExpnId::root() || ancestor == self {
            return true;
        }
        if ancestor.krate != self.krate {
            return false;
        }
        HygieneData::with(|data| data.is_descendant_of(self, ancestor))
    }

    /// `expn_id.outer_expn_is_descendant_of(ctxt)` is equivalent to but faster than
    /// `expn_id.is_descendant_of(ctxt.outer_expn())`.
    #[inline]
    pub fn outer_expn_is_descendant_of(self, ctxt: SyntaxContext) -> bool {
        HygieneData::with(|data| data.is_descendant_of(self, data.outer_expn(ctxt)))
    }

    /// Returns span for the macro which originally caused this expansion to happen.
    ///
    /// Stops backtracing at include! boundary.
    pub fn expansion_cause(mut self) -> Option<Span> {
        let mut last_macro = None;
        loop {
            // Fast path to avoid locking.
            if self == ExpnId::root() {
                break;
            }
            let expn_data = self.expn_data();
            // Stop going up the backtrace once include! is encountered
            if expn_data.kind == ExpnKind::Macro(MacroKind::Bang, sym::include) {
                break;
            }
            self = expn_data.call_site.ctxt().outer_expn();
            last_macro = Some(expn_data.call_site);
        }
        last_macro
    }
}

#[derive(Debug)]
pub(crate) struct HygieneData {
    /// Each expansion should have an associated expansion data, but sometimes there's a delay
    /// between creation of an expansion ID and obtaining its data (e.g. macros are collected
    /// first and then resolved later), so we use an `Option` here.
    local_expn_data: IndexVec<LocalExpnId, Option<ExpnData>>,
    local_expn_hashes: IndexVec<LocalExpnId, ExpnHash>,
    /// Data and hash information from external crates. We may eventually want to remove these
    /// maps, and fetch the information directly from the other crate's metadata like DefIds do.
    foreign_expn_data: FxHashMap<ExpnId, ExpnData>,
    foreign_expn_hashes: FxHashMap<ExpnId, ExpnHash>,
    expn_hash_to_expn_id: UnhashMap<ExpnHash, ExpnId>,
    syntax_context_data: Vec<SyntaxContextData>,
    syntax_context_map: FxHashMap<SyntaxContextKey, SyntaxContext>,
    /// Maps the `local_hash` of an `ExpnData` to the next disambiguator value.
    /// This is used by `update_disambiguator` to keep track of which `ExpnData`s
    /// would have collisions without a disambiguator.
    /// The keys of this map are always computed with `ExpnData.disambiguator`
    /// set to 0.
    expn_data_disambiguators: UnhashMap<Hash64, u32>,
}

impl HygieneData {
    pub(crate) fn new(edition: Edition) -> Self {
        let root_data = ExpnData::default(
            ExpnKind::Root,
            DUMMY_SP,
            edition,
            Some(CRATE_DEF_ID.to_def_id()),
            None,
        );

        let root_ctxt_data = SyntaxContextData::root();
        HygieneData {
            local_expn_data: IndexVec::from_elem_n(Some(root_data), 1),
            local_expn_hashes: IndexVec::from_elem_n(ExpnHash(Fingerprint::ZERO), 1),
            foreign_expn_data: FxHashMap::default(),
            foreign_expn_hashes: FxHashMap::default(),
            expn_hash_to_expn_id: iter::once((ExpnHash(Fingerprint::ZERO), ExpnId::root()))
                .collect(),
            syntax_context_data: vec![root_ctxt_data],
            syntax_context_map: iter::once((root_ctxt_data.key(), SyntaxContext(0))).collect(),
            expn_data_disambiguators: UnhashMap::default(),
        }
    }

    #[inline]
    fn with<R>(f: impl FnOnce(&mut HygieneData) -> R) -> R {
        with_session_globals(|session_globals| f(&mut session_globals.hygiene_data.borrow_mut()))
    }

    #[inline]
    fn expn_hash(&self, expn_id: ExpnId) -> ExpnHash {
        match expn_id.as_local() {
            Some(expn_id) => self.local_expn_hashes[expn_id],
            None => self.foreign_expn_hashes[&expn_id],
        }
    }

    #[inline]
    fn local_expn_data(&self, expn_id: LocalExpnId) -> &ExpnData {
        self.local_expn_data[expn_id].as_ref().expect("no expansion data for an expansion ID")
    }

    fn expn_data(&self, expn_id: ExpnId) -> &ExpnData {
        if let Some(expn_id) = expn_id.as_local() {
            self.local_expn_data[expn_id].as_ref().expect("no expansion data for an expansion ID")
        } else {
            &self.foreign_expn_data[&expn_id]
        }
    }

    fn is_descendant_of(&self, mut expn_id: ExpnId, ancestor: ExpnId) -> bool {
        // a couple "fast path" cases to avoid traversing parents in the loop below
        if ancestor == ExpnId::root() {
            return true;
        }
        if expn_id.krate != ancestor.krate {
            return false;
        }
        loop {
            if expn_id == ancestor {
                return true;
            }
            if expn_id == ExpnId::root() {
                return false;
            }
            expn_id = self.expn_data(expn_id).parent;
        }
    }

    #[inline]
    fn normalize_to_macros_2_0(&self, ctxt: SyntaxContext) -> SyntaxContext {
        self.syntax_context_data[ctxt.0 as usize].opaque
    }

    #[inline]
    fn normalize_to_macro_rules(&self, ctxt: SyntaxContext) -> SyntaxContext {
        self.syntax_context_data[ctxt.0 as usize].opaque_and_semiopaque
    }

    /// See [`SyntaxContextData::outer_expn`]
    #[inline]
    fn outer_expn(&self, ctxt: SyntaxContext) -> ExpnId {
        self.syntax_context_data[ctxt.0 as usize].outer_expn
    }

    /// The last macro expansion and its Transparency
    #[inline]
    fn outer_mark(&self, ctxt: SyntaxContext) -> (ExpnId, Transparency) {
        let data = &self.syntax_context_data[ctxt.0 as usize];
        (data.outer_expn, data.outer_transparency)
    }

    #[inline]
    fn parent_ctxt(&self, ctxt: SyntaxContext) -> SyntaxContext {
        self.syntax_context_data[ctxt.0 as usize].parent
    }

    fn remove_mark(&self, ctxt: &mut SyntaxContext) -> (ExpnId, Transparency) {
        let outer_mark = self.outer_mark(*ctxt);
        *ctxt = self.parent_ctxt(*ctxt);
        outer_mark
    }

    fn marks(&self, mut ctxt: SyntaxContext) -> Vec<(ExpnId, Transparency)> {
        let mut marks = Vec::new();
        while !ctxt.is_root() {
            debug!("marks: getting parent of {:?}", ctxt);
            marks.push(self.outer_mark(ctxt));
            ctxt = self.parent_ctxt(ctxt);
        }
        marks.reverse();
        marks
    }

    fn walk_chain(&self, mut span: Span, to: SyntaxContext) -> Span {
        let orig_span = span;
        debug!("walk_chain({:?}, {:?})", span, to);
        debug!("walk_chain: span ctxt = {:?}", span.ctxt());
        while span.ctxt() != to && span.from_expansion() {
            let outer_expn = self.outer_expn(span.ctxt());
            debug!("walk_chain({:?}): outer_expn={:?}", span, outer_expn);
            let expn_data = self.expn_data(outer_expn);
            debug!("walk_chain({:?}): expn_data={:?}", span, expn_data);
            span = expn_data.call_site;
        }
        debug!("walk_chain: for span {:?} >>> return span = {:?}", orig_span, span);
        span
    }

    fn walk_chain_collapsed(&self, mut span: Span, to: Span) -> Span {
        let orig_span = span;
        let mut ret_span = span;
        debug!("walk_chain_collapsed({:?}, {:?})", span, to);
        debug!("walk_chain_collapsed: span ctxt = {:?}", span.ctxt());
        while let ctxt = span.ctxt()
            && !ctxt.is_root()
            && ctxt != to.ctxt()
        {
            let outer_expn = self.outer_expn(ctxt);
            debug!("walk_chain_collapsed({:?}): outer_expn={:?}", span, outer_expn);
            let expn_data = self.expn_data(outer_expn);
            debug!("walk_chain_collapsed({:?}): expn_data={:?}", span, expn_data);
            span = expn_data.call_site;
            if expn_data.collapse_debuginfo {
                ret_span = span;
            }
        }
        debug!("walk_chain_collapsed: for span {:?} >>> return span = {:?}", orig_span, ret_span);
        ret_span
    }

    fn adjust(&self, ctxt: &mut SyntaxContext, expn_id: ExpnId) -> Option<ExpnId> {
        let mut scope = None;
        while !self.is_descendant_of(expn_id, self.outer_expn(*ctxt)) {
            scope = Some(self.remove_mark(ctxt).0);
        }
        scope
    }

    fn apply_mark(
        &mut self,
        ctxt: SyntaxContext,
        expn_id: ExpnId,
        transparency: Transparency,
    ) -> SyntaxContext {
        assert_ne!(expn_id, ExpnId::root());
        if transparency == Transparency::Opaque {
            return self.alloc_ctxt(ctxt, expn_id, transparency);
        }

        let call_site_ctxt = self.expn_data(expn_id).call_site.ctxt();
        let mut call_site_ctxt = if transparency == Transparency::SemiOpaque {
            self.normalize_to_macros_2_0(call_site_ctxt)
        } else {
            self.normalize_to_macro_rules(call_site_ctxt)
        };

        if call_site_ctxt.is_root() {
            return self.alloc_ctxt(ctxt, expn_id, transparency);
        }

        // Otherwise, `expn_id` is a macros 1.0 definition and the call site is in a
        // macros 2.0 expansion, i.e., a macros 1.0 invocation is in a macros 2.0 definition.
        //
        // In this case, the tokens from the macros 1.0 definition inherit the hygiene
        // at their invocation. That is, we pretend that the macros 1.0 definition
        // was defined at its invocation (i.e., inside the macros 2.0 definition)
        // so that the macros 2.0 definition remains hygienic.
        //
        // See the example at `test/ui/hygiene/legacy_interaction.rs`.
        for (expn_id, transparency) in self.marks(ctxt) {
            call_site_ctxt = self.alloc_ctxt(call_site_ctxt, expn_id, transparency);
        }
        self.alloc_ctxt(call_site_ctxt, expn_id, transparency)
    }

    /// Allocate a new context with the given key, or retrieve it from cache if the given key
    /// already exists. The auxiliary fields are calculated from the key.
    fn alloc_ctxt(
        &mut self,
        parent: SyntaxContext,
        expn_id: ExpnId,
        transparency: Transparency,
    ) -> SyntaxContext {
        // Look into the cache first.
        let key = (parent, expn_id, transparency);
        if let Some(ctxt) = self.syntax_context_map.get(&key) {
            return *ctxt;
        }

        // Reserve a new syntax context.
        // The inserted dummy data can only be potentially accessed by nested `alloc_ctxt` calls,
        // the assert below ensures that it doesn't happen.
        let ctxt = SyntaxContext::from_usize(self.syntax_context_data.len());
        self.syntax_context_data
            .push(SyntaxContextData { dollar_crate_name: sym::dummy, ..SyntaxContextData::root() });
        self.syntax_context_map.insert(key, ctxt);

        // Opaque and semi-opaque versions of the parent. Note that they may be equal to the
        // parent itself. E.g. `parent_opaque` == `parent` if the expn chain contains only opaques,
        // and `parent_opaque_and_semiopaque` == `parent` if the expn contains only (semi-)opaques.
        let parent_data = &self.syntax_context_data[parent.0 as usize];
        assert_ne!(parent_data.dollar_crate_name, sym::dummy);
        let parent_opaque = parent_data.opaque;
        let parent_opaque_and_semiopaque = parent_data.opaque_and_semiopaque;

        // Evaluate opaque and semi-opaque versions of the new syntax context.
        let (opaque, opaque_and_semiopaque) = match transparency {
            Transparency::Transparent => (parent_opaque, parent_opaque_and_semiopaque),
            Transparency::SemiOpaque => (
                parent_opaque,
                // Will be the same as `ctxt` if the expn chain contains only (semi-)opaques.
                self.alloc_ctxt(parent_opaque_and_semiopaque, expn_id, transparency),
            ),
            Transparency::Opaque => (
                // Will be the same as `ctxt` if the expn chain contains only opaques.
                self.alloc_ctxt(parent_opaque, expn_id, transparency),
                // Will be the same as `ctxt` if the expn chain contains only (semi-)opaques.
                self.alloc_ctxt(parent_opaque_and_semiopaque, expn_id, transparency),
            ),
        };

        // Fill the full data, now that we have it.
        self.syntax_context_data[ctxt.as_u32() as usize] = SyntaxContextData {
            outer_expn: expn_id,
            outer_transparency: transparency,
            parent,
            opaque,
            opaque_and_semiopaque,
            dollar_crate_name: kw::DollarCrate,
        };
        ctxt
    }
}

pub fn walk_chain(span: Span, to: SyntaxContext) -> Span {
    HygieneData::with(|data| data.walk_chain(span, to))
}

/// In order to have good line stepping behavior in debugger, for the given span we return its
/// outermost macro call site that still has a `#[collapse_debuginfo(yes)]` property on it.
/// We also stop walking call sites at the function body level because no line stepping can occur
/// at the level above that.
/// The returned span can then be used in emitted debuginfo.
pub fn walk_chain_collapsed(span: Span, to: Span) -> Span {
    HygieneData::with(|data| data.walk_chain_collapsed(span, to))
}

pub fn update_dollar_crate_names(mut get_name: impl FnMut(SyntaxContext) -> Symbol) {
    // The new contexts that need updating are at the end of the list and have `$crate` as a name.
    let mut to_update = vec![];
    HygieneData::with(|data| {
        for (idx, scdata) in data.syntax_context_data.iter().enumerate().rev() {
            if scdata.dollar_crate_name == kw::DollarCrate {
                to_update.push((idx, kw::DollarCrate));
            } else {
                break;
            }
        }
    });
    // The callback must be called from outside of the `HygieneData` lock,
    // since it will try to acquire it too.
    for (idx, name) in &mut to_update {
        *name = get_name(SyntaxContext::from_usize(*idx));
    }
    HygieneData::with(|data| {
        for (idx, name) in to_update {
            data.syntax_context_data[idx].dollar_crate_name = name;
        }
    })
}

pub fn debug_hygiene_data(verbose: bool) -> String {
    HygieneData::with(|data| {
        if verbose {
            format!("{data:#?}")
        } else {
            let mut s = String::from("Expansions:");
            let mut debug_expn_data = |(id, expn_data): (&ExpnId, &ExpnData)| {
                s.push_str(&format!(
                    "\n{:?}: parent: {:?}, call_site_ctxt: {:?}, def_site_ctxt: {:?}, kind: {:?}",
                    id,
                    expn_data.parent,
                    expn_data.call_site.ctxt(),
                    expn_data.def_site.ctxt(),
                    expn_data.kind,
                ))
            };
            data.local_expn_data.iter_enumerated().for_each(|(id, expn_data)| {
                let expn_data = expn_data.as_ref().expect("no expansion data for an expansion ID");
                debug_expn_data((&id.to_expn_id(), expn_data))
            });

            // Sort the hash map for more reproducible output.
            // Because of this, it is fine to rely on the unstable iteration order of the map.
            #[allow(rustc::potential_query_instability)]
            let mut foreign_expn_data: Vec<_> = data.foreign_expn_data.iter().collect();
            foreign_expn_data.sort_by_key(|(id, _)| (id.krate, id.local_id));
            foreign_expn_data.into_iter().for_each(debug_expn_data);
            s.push_str("\n\nSyntaxContexts:");
            data.syntax_context_data.iter().enumerate().for_each(|(id, ctxt)| {
                s.push_str(&format!(
                    "\n#{}: parent: {:?}, outer_mark: ({:?}, {:?})",
                    id, ctxt.parent, ctxt.outer_expn, ctxt.outer_transparency,
                ));
            });
            s
        }
    })
}

impl SyntaxContext {
    #[inline]
    pub const fn root() -> Self {
        SyntaxContext(0)
    }

    #[inline]
    pub const fn is_root(self) -> bool {
        self.0 == SyntaxContext::root().as_u32()
    }

    #[inline]
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }

    #[inline]
    pub(crate) const fn from_u32(raw: u32) -> SyntaxContext {
        SyntaxContext(raw)
    }

    #[inline]
    pub(crate) const fn from_u16(raw: u16) -> SyntaxContext {
        SyntaxContext(raw as u32)
    }

    #[inline]
    fn from_usize(raw: usize) -> SyntaxContext {
        SyntaxContext(u32::try_from(raw).unwrap())
    }

    /// Extend a syntax context with a given expansion and transparency.
    #[inline]
    pub fn apply_mark(self, expn_id: ExpnId, transparency: Transparency) -> SyntaxContext {
        HygieneData::with(|data| data.apply_mark(self, expn_id, transparency))
    }

    /// Pulls a single mark off of the syntax context. This effectively moves the
    /// context up one macro definition level. That is, if we have a nested macro
    /// definition as follows:
    ///
    /// ```ignore (illustrative)
    /// macro_rules! f {
    ///    macro_rules! g {
    ///        ...
    ///    }
    /// }
    /// ```
    ///
    /// and we have a SyntaxContext that is referring to something declared by an invocation
    /// of g (call it g1), calling remove_mark will result in the SyntaxContext for the
    /// invocation of f that created g1.
    /// Returns the mark that was removed.
    #[inline]
    pub fn remove_mark(&mut self) -> ExpnId {
        HygieneData::with(|data| data.remove_mark(self).0)
    }

    #[inline]
    pub fn marks(self) -> Vec<(ExpnId, Transparency)> {
        HygieneData::with(|data| data.marks(self))
    }

    /// Adjust this context for resolution in a scope created by the given expansion.
    /// For example, consider the following three resolutions of `f`:
    ///
    /// ```rust
    /// #![feature(decl_macro)]
    /// mod foo {
    ///     pub fn f() {} // `f`'s `SyntaxContext` is empty.
    /// }
    /// m!(f);
    /// macro m($f:ident) {
    ///     mod bar {
    ///         pub fn f() {} // `f`'s `SyntaxContext` has a single `ExpnId` from `m`.
    ///         pub fn $f() {} // `$f`'s `SyntaxContext` is empty.
    ///     }
    ///     foo::f(); // `f`'s `SyntaxContext` has a single `ExpnId` from `m`
    ///     //^ Since `mod foo` is outside this expansion, `adjust` removes the mark from `f`,
    ///     //| and it resolves to `::foo::f`.
    ///     bar::f(); // `f`'s `SyntaxContext` has a single `ExpnId` from `m`
    ///     //^ Since `mod bar` not outside this expansion, `adjust` does not change `f`,
    ///     //| and it resolves to `::bar::f`.
    ///     bar::$f(); // `f`'s `SyntaxContext` is empty.
    ///     //^ Since `mod bar` is not outside this expansion, `adjust` does not change `$f`,
    ///     //| and it resolves to `::bar::$f`.
    /// }
    /// ```
    /// This returns the expansion whose definition scope we use to privacy check the resolution,
    /// or `None` if we privacy check as usual (i.e., not w.r.t. a macro definition scope).
    #[inline]
    pub fn adjust(&mut self, expn_id: ExpnId) -> Option<ExpnId> {
        HygieneData::with(|data| data.adjust(self, expn_id))
    }

    /// Like `SyntaxContext::adjust`, but also normalizes `self` to macros 2.0.
    #[inline]
    pub fn normalize_to_macros_2_0_and_adjust(&mut self, expn_id: ExpnId) -> Option<ExpnId> {
        HygieneData::with(|data| {
            *self = data.normalize_to_macros_2_0(*self);
            data.adjust(self, expn_id)
        })
    }

    /// Adjust this context for resolution in a scope created by the given expansion
    /// via a glob import with the given `SyntaxContext`.
    /// For example:
    ///
    /// ```compile_fail,E0425
    /// #![feature(decl_macro)]
    /// m!(f);
    /// macro m($i:ident) {
    ///     mod foo {
    ///         pub fn f() {} // `f`'s `SyntaxContext` has a single `ExpnId` from `m`.
    ///         pub fn $i() {} // `$i`'s `SyntaxContext` is empty.
    ///     }
    ///     n!(f);
    ///     macro n($j:ident) {
    ///         use foo::*;
    ///         f(); // `f`'s `SyntaxContext` has a mark from `m` and a mark from `n`
    ///         //^ `glob_adjust` removes the mark from `n`, so this resolves to `foo::f`.
    ///         $i(); // `$i`'s `SyntaxContext` has a mark from `n`
    ///         //^ `glob_adjust` removes the mark from `n`, so this resolves to `foo::$i`.
    ///         $j(); // `$j`'s `SyntaxContext` has a mark from `m`
    ///         //^ This cannot be glob-adjusted, so this is a resolution error.
    ///     }
    /// }
    /// ```
    /// This returns `None` if the context cannot be glob-adjusted.
    /// Otherwise, it returns the scope to use when privacy checking (see `adjust` for details).
    pub fn glob_adjust(&mut self, expn_id: ExpnId, glob_span: Span) -> Option<Option<ExpnId>> {
        HygieneData::with(|data| {
            let mut scope = None;
            let mut glob_ctxt = data.normalize_to_macros_2_0(glob_span.ctxt());
            while !data.is_descendant_of(expn_id, data.outer_expn(glob_ctxt)) {
                scope = Some(data.remove_mark(&mut glob_ctxt).0);
                if data.remove_mark(self).0 != scope.unwrap() {
                    return None;
                }
            }
            if data.adjust(self, expn_id).is_some() {
                return None;
            }
            Some(scope)
        })
    }

    /// Undo `glob_adjust` if possible:
    ///
    /// ```ignore (illustrative)
    /// if let Some(privacy_checking_scope) = self.reverse_glob_adjust(expansion, glob_ctxt) {
    ///     assert!(self.glob_adjust(expansion, glob_ctxt) == Some(privacy_checking_scope));
    /// }
    /// ```
    pub fn reverse_glob_adjust(
        &mut self,
        expn_id: ExpnId,
        glob_span: Span,
    ) -> Option<Option<ExpnId>> {
        HygieneData::with(|data| {
            if data.adjust(self, expn_id).is_some() {
                return None;
            }

            let mut glob_ctxt = data.normalize_to_macros_2_0(glob_span.ctxt());
            let mut marks = Vec::new();
            while !data.is_descendant_of(expn_id, data.outer_expn(glob_ctxt)) {
                marks.push(data.remove_mark(&mut glob_ctxt));
            }

            let scope = marks.last().map(|mark| mark.0);
            while let Some((expn_id, transparency)) = marks.pop() {
                *self = data.apply_mark(*self, expn_id, transparency);
            }
            Some(scope)
        })
    }

    pub fn hygienic_eq(self, other: SyntaxContext, expn_id: ExpnId) -> bool {
        HygieneData::with(|data| {
            let mut self_normalized = data.normalize_to_macros_2_0(self);
            data.adjust(&mut self_normalized, expn_id);
            self_normalized == data.normalize_to_macros_2_0(other)
        })
    }

    #[inline]
    pub fn normalize_to_macros_2_0(self) -> SyntaxContext {
        HygieneData::with(|data| data.normalize_to_macros_2_0(self))
    }

    #[inline]
    pub fn normalize_to_macro_rules(self) -> SyntaxContext {
        HygieneData::with(|data| data.normalize_to_macro_rules(self))
    }

    /// See [`SyntaxContextData::outer_expn`]
    #[inline]
    pub fn outer_expn(self) -> ExpnId {
        HygieneData::with(|data| data.outer_expn(self))
    }

    /// `ctxt.outer_expn_data()` is equivalent to but faster than
    /// `ctxt.outer_expn().expn_data()`.
    #[inline]
    pub fn outer_expn_data(self) -> ExpnData {
        HygieneData::with(|data| data.expn_data(data.outer_expn(self)).clone())
    }

    /// See [`HygieneData::outer_mark`]
    #[inline]
    fn outer_mark(self) -> (ExpnId, Transparency) {
        HygieneData::with(|data| data.outer_mark(self))
    }

    #[inline]
    pub(crate) fn dollar_crate_name(self) -> Symbol {
        HygieneData::with(|data| data.syntax_context_data[self.0 as usize].dollar_crate_name)
    }

    #[inline]
    pub fn edition(self) -> Edition {
        HygieneData::with(|data| data.expn_data(data.outer_expn(self)).edition)
    }

    /// Returns whether this context originates in a foreign crate's external macro.
    ///
    /// This is used to test whether a lint should not even begin to figure out whether it should
    /// be reported on the current node.
    pub fn in_external_macro(self, sm: &SourceMap) -> bool {
        let expn_data = self.outer_expn_data();
        match expn_data.kind {
            ExpnKind::Root
            | ExpnKind::Desugaring(
                DesugaringKind::ForLoop
                | DesugaringKind::WhileLoop
                | DesugaringKind::OpaqueTy
                | DesugaringKind::Async
                | DesugaringKind::Await,
            ) => false,
            ExpnKind::AstPass(_) | ExpnKind::Desugaring(_) => true, // well, it's "external"
            ExpnKind::Macro(MacroKind::Bang, _) => {
                // Dummy span for the `def_site` means it's an external macro.
                expn_data.def_site.is_dummy() || sm.is_imported(expn_data.def_site)
            }
            ExpnKind::Macro { .. } => true, // definitely a plugin
        }
    }
}

impl fmt::Debug for SyntaxContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl Span {
    /// Reuses the span but adds information like the kind of the desugaring and features that are
    /// allowed inside this span.
    pub fn mark_with_reason(
        self,
        allow_internal_unstable: Option<Arc<[Symbol]>>,
        reason: DesugaringKind,
        edition: Edition,
        hcx: impl StableHashCtxt,
    ) -> Span {
        let expn_data = ExpnData {
            allow_internal_unstable,
            ..ExpnData::default(ExpnKind::Desugaring(reason), self, edition, None, None)
        };
        let expn_id = LocalExpnId::fresh(expn_data, hcx);
        self.apply_mark(expn_id.to_expn_id(), Transparency::Transparent)
    }
}

/// A subset of properties from both macro definition and macro call available through global data.
/// Avoid using this if you have access to the original definition or call structures.
#[derive(Clone, Debug, Encodable, Decodable, StableHash)]
pub struct ExpnData {
    // --- The part unique to each expansion.
    pub kind: ExpnKind,
    /// The expansion that contains the definition of the macro for this expansion.
    pub parent: ExpnId,
    /// The span of the macro call which produced this expansion.
    ///
    /// This span will typically have a different `ExpnData` and `call_site`.
    /// This recursively traces back through any macro calls which expanded into further
    /// macro calls, until the "source call-site" is reached at the root SyntaxContext.
    /// For example, if `food!()` expands to `fruit!()` which then expands to `grape`,
    /// then the call-site of `grape` is `fruit!()` and the call-site of `fruit!()`
    /// is `food!()`.
    ///
    /// For a desugaring expansion, this is the span of the expression or node that was
    /// desugared.
    pub call_site: Span,
    /// Used to force two `ExpnData`s to have different `Fingerprint`s.
    /// Due to macro expansion, it's possible to end up with two `ExpnId`s
    /// that have identical `ExpnData`s. This violates the contract of `StableHash`
    /// - the two `ExpnId`s are not equal, but their `Fingerprint`s are equal
    /// (since the numerical `ExpnId` value is not considered by the `StableHash`
    /// implementation).
    ///
    /// The `disambiguator` field is set by `update_disambiguator` when two distinct
    /// `ExpnId`s would end up with the same `Fingerprint`. Since `ExpnData` includes
    /// a `krate` field, this value only needs to be unique within a single crate.
    disambiguator: u32,

    // --- The part specific to the macro/desugaring definition.
    // --- It may be reasonable to share this part between expansions with the same definition,
    // --- but such sharing is known to bring some minor inconveniences without also bringing
    // --- noticeable perf improvements (PR #62898).
    /// The span of the macro definition (possibly dummy).
    /// This span serves only informational purpose and is not used for resolution.
    pub def_site: Span,
    /// List of `#[unstable]`/feature-gated features that the macro is allowed to use
    /// internally without forcing the whole crate to opt-in
    /// to them.
    pub allow_internal_unstable: Option<Arc<[Symbol]>>,
    /// Edition of the crate in which the macro is defined.
    pub edition: Edition,
    /// The `DefId` of the macro being invoked,
    /// if this `ExpnData` corresponds to a macro invocation
    pub macro_def_id: Option<DefId>,
    /// The normal module (`mod`) in which the expanded macro was defined.
    pub parent_module: Option<ModId>,
    /// Suppresses the `unsafe_code` lint for code produced by this macro.
    pub(crate) allow_internal_unsafe: bool,
    /// Enables the macro helper hack (`ident!(...)` -> `$crate::ident!(...)`) for this macro.
    pub local_inner_macros: bool,
    /// Should debuginfo for the macro be collapsed to the outermost expansion site (in other
    /// words, was the macro definition annotated with `#[collapse_debuginfo]`)?
    pub(crate) collapse_debuginfo: bool,
    /// When true, we prevent diagnostics pointing into this macro, if it is one, and we do not
    /// display the note telling people to use the `-Zmacro-backtrace` flag.
    pub diagnostic_opaque: bool,
}

impl !PartialEq for ExpnData {}
impl !Hash for ExpnData {}

impl ExpnData {
    pub fn new(
        kind: ExpnKind,
        parent: ExpnId,
        call_site: Span,
        def_site: Span,
        allow_internal_unstable: Option<Arc<[Symbol]>>,
        edition: Edition,
        macro_def_id: Option<DefId>,
        parent_module: Option<ModId>,
        allow_internal_unsafe: bool,
        local_inner_macros: bool,
        collapse_debuginfo: bool,
        diagnostic_opaque: bool,
    ) -> ExpnData {
        ExpnData {
            kind,
            parent,
            call_site,
            def_site,
            allow_internal_unstable,
            edition,
            macro_def_id,
            parent_module,
            disambiguator: 0,
            allow_internal_unsafe,
            local_inner_macros,
            collapse_debuginfo,
            diagnostic_opaque,
        }
    }

    /// Constructs expansion data with default properties.
    pub fn default(
        kind: ExpnKind,
        call_site: Span,
        edition: Edition,
        macro_def_id: Option<DefId>,
        parent_module: Option<ModId>,
    ) -> ExpnData {
        ExpnData {
            kind,
            parent: ExpnId::root(),
            call_site,
            def_site: DUMMY_SP,
            allow_internal_unstable: None,
            edition,
            macro_def_id,
            parent_module,
            disambiguator: 0,
            allow_internal_unsafe: false,
            local_inner_macros: false,
            collapse_debuginfo: false,
            diagnostic_opaque: false,
        }
    }

    pub fn allow_unstable(
        kind: ExpnKind,
        call_site: Span,
        edition: Edition,
        allow_internal_unstable: Arc<[Symbol]>,
        macro_def_id: Option<DefId>,
        parent_module: Option<ModId>,
    ) -> ExpnData {
        ExpnData {
            allow_internal_unstable: Some(allow_internal_unstable),
            ..ExpnData::default(kind, call_site, edition, macro_def_id, parent_module)
        }
    }

    #[inline]
    pub fn is_root(&self) -> bool {
        matches!(self.kind, ExpnKind::Root)
    }

    /// Hashes cross-artifact expansion meaning without the session collision disambiguator.
    ///
    /// The ordinary disambiguator depends on which equivalent expansions the current session
    /// allocated first. RDR assigns duplicate identities from canonical metadata provenance, so
    /// session allocation history must not enter this hash.
    pub fn stable_hash_artifact_semantics<Hcx: StableHashCtxt>(
        &self,
        hcx: &mut Hcx,
        hasher: &mut StableHasher,
    ) {
        self.for_metadata_artifact().stable_hash(hcx, hasher);
    }

    fn for_metadata_artifact(&self) -> Self {
        let mut data = self.clone();
        data.disambiguator = 0;
        data
    }

    #[inline]
    fn hash_expn(&self, hcx: &mut impl StableHashCtxt) -> Hash64 {
        let mut hasher = StableHasher::new();
        self.stable_hash(hcx, &mut hasher);
        hasher.finish()
    }
}

/// Expansion kind.
#[derive(Clone, Debug, PartialEq, Encodable, Decodable, StableHash)]
pub enum ExpnKind {
    /// No expansion, aka root expansion. Only `ExpnId::root()` has this kind.
    Root,
    /// Expansion produced by a macro.
    Macro(MacroKind, Symbol),
    /// Transform done by the compiler on the AST.
    AstPass(AstPass),
    /// Desugaring done by the compiler during AST lowering.
    Desugaring(DesugaringKind),
}

impl ExpnKind {
    pub fn descr(&self) -> String {
        match *self {
            ExpnKind::Root => kw::PathRoot.to_string(),
            ExpnKind::Macro(macro_kind, name) => match macro_kind {
                MacroKind::Bang => format!("{name}!"),
                MacroKind::Attr => format!("#[{name}]"),
                MacroKind::Derive => format!("#[derive({name})]"),
            },
            ExpnKind::AstPass(kind) => kind.descr().to_string(),
            ExpnKind::Desugaring(kind) => format!("desugaring of {}", kind.descr()),
        }
    }
}

/// The kind of macro invocation or definition.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Encodable, Decodable, Hash, Debug)]
#[derive(StableHash)]
pub enum MacroKind {
    /// A bang macro `foo!()`.
    Bang,
    /// An attribute macro `#[foo]`.
    Attr,
    /// A derive macro `#[derive(Foo)]`
    Derive,
}

impl MacroKind {
    pub fn descr(self) -> &'static str {
        match self {
            MacroKind::Bang => "macro",
            MacroKind::Attr => "attribute macro",
            MacroKind::Derive => "derive macro",
        }
    }

    pub fn descr_expected(self) -> &'static str {
        match self {
            MacroKind::Attr => "attribute",
            _ => self.descr(),
        }
    }

    pub fn article(self) -> &'static str {
        match self {
            MacroKind::Attr => "an",
            _ => "a",
        }
    }
}

/// The kind of AST transform.
#[derive(Clone, Copy, Debug, PartialEq, Encodable, Decodable, StableHash)]
pub enum AstPass {
    StdImports,
    TestHarness,
    ProcMacroHarness,
}

impl AstPass {
    pub fn descr(self) -> &'static str {
        match self {
            AstPass::StdImports => "standard library imports",
            AstPass::TestHarness => "test harness",
            AstPass::ProcMacroHarness => "proc macro harness",
        }
    }
}

/// The kind of compiler desugaring.
#[derive(Clone, Copy, PartialEq, Debug, Encodable, Decodable, StableHash)]
pub enum DesugaringKind {
    QuestionMark,
    TryBlock,
    YeetExpr,
    /// Desugaring of an `impl Trait` in return type position
    /// to an `type Foo = impl Trait;` and replacing the
    /// `impl Trait` with `Foo`.
    OpaqueTy,
    Async,
    Await,
    ForLoop,
    WhileLoop,
    /// `async Fn()` bound modifier
    BoundModifier,
    /// Calls to contract checks (`#[requires]` to precond, `#[ensures]` to postcond)
    Contract,
    /// A pattern type range start/end
    PatTyRange,
    /// A format literal.
    FormatLiteral {
        /// Was this format literal written in the source?
        /// - `format!("boo")` => Yes,
        /// - `format!(concat!("b", "o", "o"))` => No,
        /// - `format!(include_str!("boo.txt"))` => No,
        ///
        /// If it wasn't written in the source then we have to be careful with suggestions about
        /// rewriting it.
        source: bool,
    },
    RangeExpr,
}

impl DesugaringKind {
    /// The description wording should combine well with "desugaring of {}".
    pub fn descr(self) -> &'static str {
        match self {
            DesugaringKind::Async => "`async` block or function",
            DesugaringKind::Await => "`await` expression",
            DesugaringKind::QuestionMark => "operator `?`",
            DesugaringKind::TryBlock => "`try` block",
            DesugaringKind::YeetExpr => "`do yeet` expression",
            DesugaringKind::OpaqueTy => "`impl Trait`",
            DesugaringKind::ForLoop => "`for` loop",
            DesugaringKind::WhileLoop => "`while` loop",
            DesugaringKind::BoundModifier => "trait bound modifier",
            DesugaringKind::Contract => "contract check",
            DesugaringKind::PatTyRange => "pattern type",
            DesugaringKind::FormatLiteral { source: true } => "format string literal",
            DesugaringKind::FormatLiteral { source: false } => {
                "expression that expanded into a format string literal"
            }
            DesugaringKind::RangeExpr => "range expression",
        }
    }

    /// For use with `rustc_unimplemented` to support conditions
    /// like `from_desugaring = "QuestionMark"`
    pub fn matches(&self, value: &str) -> bool {
        match self {
            DesugaringKind::Async => value == "Async",
            DesugaringKind::Await => value == "Await",
            DesugaringKind::QuestionMark => value == "QuestionMark",
            DesugaringKind::TryBlock => value == "TryBlock",
            DesugaringKind::YeetExpr => value == "YeetExpr",
            DesugaringKind::OpaqueTy => value == "OpaqueTy",
            DesugaringKind::ForLoop => value == "ForLoop",
            DesugaringKind::WhileLoop => value == "WhileLoop",
            DesugaringKind::BoundModifier => value == "BoundModifier",
            DesugaringKind::Contract => value == "Contract",
            DesugaringKind::PatTyRange => value == "PatTyRange",
            DesugaringKind::FormatLiteral { .. } => value == "FormatLiteral",
            DesugaringKind::RangeExpr => value == "RangeExpr",
        }
    }
}

pub struct HygieneEncodeContext {
    // All syntax contexts emitted by this context. A frontier returns only the identities newly
    // inserted into this set.
    serialized_ctxts: Lock<UnordSet<SyntaxContext>>,
    // Syntax contexts scheduled during the current round. Encoding their data may schedule more
    // contexts, so `encode_pending` drains rounds until this set remains empty.
    latest_ctxts: Lock<UnordSet<SyntaxContext>>,

    // Expansions use the same emitted/scheduled fixed-point protocol as syntax contexts.
    serialized_expns: Lock<UnordSet<ExpnId>>,
    latest_expns: Lock<UnordSet<ExpnId>>,

    mode: HygieneEncodeMode,
}

enum HygieneEncodeMode {
    SessionLocal,
    Artifact(HygieneEncodeLayout),
}

impl Default for HygieneEncodeContext {
    fn default() -> Self {
        Self {
            serialized_ctxts: Default::default(),
            latest_ctxts: Default::default(),
            serialized_expns: Default::default(),
            latest_expns: Default::default(),
            mode: HygieneEncodeMode::SessionLocal,
        }
    }
}

#[derive(Eq, PartialEq)]
struct SyntaxContextStableKey<T>(Vec<(T, u8)>);

impl<T: Ord> StableCompare for SyntaxContextStableKey<T> {
    const CAN_USE_UNSTABLE_SORT: bool = true;

    fn stable_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// Separates content-derived hygiene meaning from session and wire-format addresses.
///
/// The fingerprint remains private so metadata cannot accidentally substitute an ordinary hash
/// or persist a dense artifact index where semantic identity is required.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, StableHash)]
pub struct HygieneIdentity(Fingerprint);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactSyntaxContext {
    index: u32,
    identity: HygieneIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArtifactExpansion {
    index: ExpnIndex,
    identity: HygieneIdentity,
    hash: ExpnHash,
}

/// Maps content-derived hygiene identities to one deterministic artifact address space.
///
/// Projection owns this value so emission cannot derive a second set of identities. Dense indices
/// remain wire addresses only; stable hashing uses the identities stored beside them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HygieneEncodeLayout {
    syntax_contexts: Arc<FxHashMap<SyntaxContext, ArtifactSyntaxContext>>,
    expansions: Arc<FxHashMap<ExpnId, ArtifactExpansion>>,
}

impl HygieneEncodeLayout {
    /// Runs a hygiene trace and closes its reached graph into an immutable layout.
    ///
    /// The trace handle cannot outlive this call. This keeps an open trace from being reused after
    /// its identities have been assigned.
    pub fn trace<Hcx: StableHashCtxt, R>(
        hcx: &mut Hcx,
        trace: impl FnOnce(&HygieneTrace) -> R,
    ) -> (R, Self) {
        let hygiene = HygieneTrace::new();
        let result = trace(&hygiene);
        let HygieneTrace { reached, roots } = hygiene;
        let layout = HygieneIdentityBuilder::build(hcx, reached.into_inner(), roots.into_inner());
        (result, layout)
    }

    fn syntax_context(&self, ctxt: SyntaxContext) -> ArtifactSyntaxContext {
        self.syntax_contexts.get(&ctxt).copied().unwrap_or_else(|| {
            panic!("metadata emission discovered an unprojected syntax context {ctxt:?}")
        })
    }

    fn expansion(&self, expn: ExpnId) -> ArtifactExpansion {
        self.expansions.get(&expn).copied().unwrap_or_else(|| {
            panic!("metadata emission discovered an unprojected expansion {expn:?}")
        })
    }

    /// Hashes an expansion through the identity assigned by projection.
    pub fn stable_hash_expansion<Hcx: StableHashCtxt>(
        &self,
        expn: ExpnId,
        hcx: &mut Hcx,
        hasher: &mut StableHasher,
    ) {
        self.expansion(expn).identity.stable_hash(hcx, hasher);
    }

    /// Verifies that emission reached exactly the graph closed by projection.
    pub fn assert_reached(&self, delta: HygieneDelta) {
        let HygieneDelta { syntax_contexts, expansions } = delta;
        let mut syntax_contexts = syntax_contexts;
        syntax_contexts.insert(SyntaxContext::root());
        let mut expansions = expansions;
        expansions.insert(ExpnId::root());
        assert_eq!(
            syntax_contexts.len(),
            self.syntax_contexts.len(),
            "canonical metadata emission changed its syntax-context closure"
        );
        assert_eq!(
            expansions.len(),
            self.expansions.len(),
            "canonical metadata emission changed its expansion closure"
        );
        assert!(
            syntax_contexts.into_items().all(|ctxt| self.syntax_contexts.contains_key(&ctxt)),
            "canonical metadata emission changed its syntax-context closure"
        );
        assert!(
            expansions.into_items().all(|expn| self.expansions.contains_key(&expn)),
            "canonical metadata emission changed its expansion closure"
        );
    }
}

/// Owns the identities reached by one hygiene frontier.
#[derive(Default)]
pub struct HygieneDelta {
    syntax_contexts: UnordSet<SyntaxContext>,
    expansions: UnordSet<ExpnId>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum HygieneNode {
    SyntaxContext(SyntaxContext),
    Expansion(ExpnId),
}

/// Collects the closed hygiene graph and its canonical metadata entry points.
///
/// Callers can add observations only while [`HygieneEncodeLayout::trace`] is running. The layout
/// then consumes the private trace state, so identity assignment and later emission cannot race.
pub struct HygieneTrace {
    reached: Lock<HygieneDelta>,
    roots: Lock<UnordMap<HygieneNode, Fingerprint>>,
}

impl HygieneTrace {
    fn new() -> Self {
        Self { reached: Lock::new(HygieneDelta::default()), roots: Lock::new(UnordMap::default()) }
    }

    /// Records a metadata occurrence that directly names a syntax context.
    pub fn observe_syntax_context(&self, ctxt: SyntaxContext, occurrence: Fingerprint) {
        if ctxt.is_root() {
            return;
        }
        Self::observe(&mut self.roots.lock(), HygieneNode::SyntaxContext(ctxt), occurrence);
    }

    /// Records a metadata occurrence that directly names a local expansion.
    pub fn observe_expansion(&self, expn: ExpnId, occurrence: Fingerprint) {
        if expn == ExpnId::root() || expn.krate != LOCAL_CRATE {
            return;
        }
        Self::observe(&mut self.roots.lock(), HygieneNode::Expansion(expn), occurrence);
    }

    /// Adds the identities discovered by one hygiene encoding frontier.
    pub fn extend(&self, delta: HygieneDelta) {
        let HygieneDelta { syntax_contexts, expansions } = delta;
        let mut reached = self.reached.lock();
        reached.syntax_contexts.extend_unord(syntax_contexts.into_items());
        reached.expansions.extend_unord(expansions.into_items());
    }

    fn observe(
        roots: &mut UnordMap<HygieneNode, Fingerprint>,
        node: HygieneNode,
        occurrence: Fingerprint,
    ) {
        roots
            .entry(node)
            .and_modify(|current| *current = (*current).min(occurrence))
            .or_insert(occurrence);
    }
}

/// Keeps a syntax context's artifact address, payload, and identity in one encoding mode.
pub struct HygieneEncodedSyntaxContext {
    index: u32,
    data: SyntaxContextKey,
    identity: HygieneIdentity,
}

impl HygieneEncodedSyntaxContext {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn data(&self) -> &SyntaxContextKey {
        &self.data
    }

    pub fn identity(&self) -> HygieneIdentity {
        self.identity
    }
}

/// Keeps an expansion's artifact address, payload, and identity in one encoding mode.
///
/// RDR replaces session-local addresses and coordinate-sensitive hashes as one operation.
pub struct HygieneEncodedExpansion {
    id: ExpnId,
    index: ExpnIndex,
    data: ExpnData,
    hash: ExpnHash,
    identity: HygieneIdentity,
}

impl HygieneEncodedExpansion {
    pub fn id(&self) -> ExpnId {
        self.id
    }

    pub fn index(&self) -> ExpnIndex {
        self.index
    }

    pub fn data(&self) -> &ExpnData {
        &self.data
    }

    pub fn hash(&self) -> ExpnHash {
        self.hash
    }

    pub fn identity(&self) -> HygieneIdentity {
        self.identity
    }
}

impl HygieneEncodeContext {
    /// Makes encoding reject hygiene identities absent from the projected layout.
    pub fn with_layout(layout: HygieneEncodeLayout) -> Self {
        Self { mode: HygieneEncodeMode::Artifact(layout), ..Self::default() }
    }

    /// Returns the closed identity space required by artifact encoding.
    pub fn artifact_layout(&self) -> &HygieneEncodeLayout {
        match &self.mode {
            HygieneEncodeMode::Artifact(layout) => layout,
            HygieneEncodeMode::SessionLocal => {
                panic!("session-local hygiene encoding has no artifact layout")
            }
        }
    }

    fn syntax_context_stable_key<T>(
        data: &HygieneData,
        ctxt: SyntaxContext,
        mut expansion_key: impl FnMut(ExpnId) -> T,
    ) -> SyntaxContextStableKey<T> {
        let mut stable_key = Vec::new();
        let mut current = ctxt;
        while !current.is_root() {
            let context = &data.syntax_context_data[current.0 as usize];
            let transparency = match context.outer_transparency {
                Transparency::Transparent => 0,
                Transparency::SemiOpaque => 1,
                Transparency::Opaque => 2,
            };
            stable_key.push((expansion_key(context.outer_expn), transparency));
            current = context.parent;
        }
        stable_key.reverse();
        SyntaxContextStableKey(stable_key)
    }

    fn encoded_expansion(
        &self,
        id: ExpnId,
        data: ExpnData,
        hash: ExpnHash,
    ) -> HygieneEncodedExpansion {
        match &self.mode {
            HygieneEncodeMode::SessionLocal => HygieneEncodedExpansion {
                id,
                index: id.local_id,
                data,
                hash,
                identity: HygieneIdentity(hash.0),
            },
            HygieneEncodeMode::Artifact(layout) if id.krate == LOCAL_CRATE => {
                let artifact = layout.expansion(id);
                let data = data.for_metadata_artifact();
                HygieneEncodedExpansion {
                    id,
                    index: artifact.index,
                    data,
                    hash: artifact.hash,
                    identity: artifact.identity,
                }
            }
            HygieneEncodeMode::Artifact(_) => HygieneEncodedExpansion {
                id,
                index: id.local_id,
                data,
                hash,
                identity: HygieneIdentity(hash.0),
            },
        }
    }

    pub fn syntax_context_index(&self, ctxt: SyntaxContext) -> u32 {
        match &self.mode {
            HygieneEncodeMode::SessionLocal => ctxt.0,
            HygieneEncodeMode::Artifact(layout) => layout.syntax_context(ctxt).index,
        }
    }

    pub fn expansion_index(&self, expn: ExpnId) -> ExpnIndex {
        if expn.krate != LOCAL_CRATE {
            return expn.local_id;
        }
        match &self.mode {
            HygieneEncodeMode::SessionLocal => expn.local_id,
            HygieneEncodeMode::Artifact(layout) => layout.expansion(expn).index,
        }
    }

    /// Record the fact that we need to serialize the corresponding `ExpnData`.
    pub fn schedule_expn_data_for_encoding(&self, expn: ExpnId) {
        if !self.serialized_expns.lock().contains(&expn) {
            self.latest_expns.lock().insert(expn);
        }
    }

    /// Drains identities scheduled by the preceding metadata frontier.
    pub fn encode_pending<T>(
        &self,
        encoder: &mut T,
        mut encode_ctxt: impl FnMut(&mut T, &HygieneEncodedSyntaxContext),
        mut encode_expn: impl FnMut(&mut T, &HygieneEncodedExpansion),
    ) -> HygieneDelta {
        let mut encoded_ctxts = UnordSet::default();
        let mut encoded_expns = UnordSet::default();
        // When we serialize a `SyntaxContextData`, we may end up serializing
        // a `SyntaxContext` that we haven't seen before
        while !self.latest_ctxts.lock().is_empty() || !self.latest_expns.lock().is_empty() {
            debug!(
                "encode_hygiene: Serializing a round of {:?} SyntaxContextData: {:?}",
                self.latest_ctxts.lock().len(),
                self.latest_ctxts
            );

            let latest_ctxts = mem::take(&mut *self.latest_ctxts.lock()).into_items();
            let all_ctxt_data: Vec<_> = HygieneData::with(|data| match &self.mode {
                HygieneEncodeMode::SessionLocal => latest_ctxts
                    .map(|ctxt| {
                        let stable_key = Self::syntax_context_stable_key(data, ctxt, |expn| {
                            data.expn_hash(expn).0
                        });
                        (stable_key, ctxt, data.syntax_context_data[ctxt.0 as usize].key())
                    })
                    .into_sorted_stable_ord_by_key(|(stable_key, _, _)| stable_key)
                    .into_iter()
                    .map(|(stable_key, ctxt, data)| {
                        let mut hasher = StableHasher::new();
                        stable_key.0.hash(&mut hasher);
                        (
                            ctxt,
                            HygieneEncodedSyntaxContext {
                                index: ctxt.0,
                                data,
                                identity: HygieneIdentity(hasher.finish()),
                            },
                        )
                    })
                    .collect(),
                HygieneEncodeMode::Artifact(layout) => latest_ctxts
                    .map(|ctxt| {
                        let artifact = layout.syntax_context(ctxt);
                        (
                            artifact.index,
                            ctxt,
                            data.syntax_context_data[ctxt.0 as usize].key(),
                            artifact.identity,
                        )
                    })
                    .into_sorted_stable_ord_by_key(|(index, _, _, _)| index)
                    .into_iter()
                    .map(|(index, ctxt, data, identity)| {
                        (ctxt, HygieneEncodedSyntaxContext { index, data, identity })
                    })
                    .collect(),
            });
            for (ctxt, context) in all_ctxt_data {
                if self.serialized_ctxts.lock().insert(ctxt) {
                    encoded_ctxts.insert(ctxt);
                    encode_ctxt(encoder, &context);
                }
            }

            let latest_expns = mem::take(&mut *self.latest_expns.lock()).into_items();
            let all_expn_data = HygieneData::with(|data| {
                let expansions = latest_expns.map(|expn| {
                    self.encoded_expansion(expn, data.expn_data(expn).clone(), data.expn_hash(expn))
                });
                match &self.mode {
                    HygieneEncodeMode::SessionLocal => {
                        expansions.into_sorted_stable_ord_by_key(|expansion| &expansion.hash)
                    }
                    HygieneEncodeMode::Artifact(_) => {
                        expansions.into_sorted_stable_ord_by_key(|expansion| &expansion.identity.0)
                    }
                }
            });
            for expansion in all_expn_data {
                if self.serialized_expns.lock().insert(expansion.id) {
                    encoded_expns.insert(expansion.id);
                    encode_expn(encoder, &expansion);
                }
            }
        }
        debug!("encode_hygiene: Done serializing SyntaxContextData");
        HygieneDelta { syntax_contexts: encoded_ctxts, expansions: encoded_expns }
    }
}

struct HygieneGraph {
    syntax_contexts: FxHashMap<SyntaxContext, SyntaxContextKey>,
    expansions: FxHashMap<ExpnId, ExpnData>,
    syntax_context_order: Vec<SyntaxContext>,
    expansion_order: Vec<ExpnId>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HygieneProvenance {
    root: Fingerprint,
    edges: Vec<HygieneEdge>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum HygieneEdge {
    ContextParent,
    ContextExpansion,
    ExpansionParent,
    ExpansionCallSite,
    ExpansionDefSite,
}

#[derive(Clone, Copy)]
enum ExpansionIdentity<'a> {
    Structural,
    Disambiguated(&'a FxHashMap<ExpnId, Option<u32>>),
}

struct HygieneIdentityBuilder<'a, Hcx> {
    graph: &'a HygieneGraph,
    hcx: &'a mut Hcx,
    expansion_identity: ExpansionIdentity<'a>,
    syntax_contexts: FxHashMap<SyntaxContext, Fingerprint>,
    expansions: FxHashMap<ExpnId, Fingerprint>,
    visiting_syntax_contexts: FxHashSet<SyntaxContext>,
    visiting_expansions: FxHashSet<ExpnId>,
}

impl<Hcx: StableHashCtxt> HygieneIdentityBuilder<'_, Hcx> {
    fn build(
        hcx: &mut Hcx,
        delta: HygieneDelta,
        roots: UnordMap<HygieneNode, Fingerprint>,
    ) -> HygieneEncodeLayout {
        let HygieneDelta { syntax_contexts, expansions } = delta;
        let syntax_contexts: Vec<_> = syntax_contexts
            .into_items()
            .filter(|ctxt| !ctxt.is_root())
            .map(|ctxt| (ctxt.0, ctxt))
            .into_sorted_stable_ord_by_key(|(index, _)| index)
            .into_iter()
            .map(|(_, ctxt)| ctxt)
            .collect();
        let expansions: Vec<_> = expansions
            .into_items()
            .filter(|expn| *expn != ExpnId::root())
            .map(|expn| {
                assert_eq!(
                    expn.krate, LOCAL_CRATE,
                    "foreign expansion entered a local metadata hygiene layout"
                );
                (expn.local_id.as_u32(), expn)
            })
            .into_sorted_stable_ord_by_key(|(index, _)| index)
            .into_iter()
            .map(|(_, expn)| expn)
            .collect();
        let graph = HygieneData::with(|data| HygieneGraph {
            syntax_contexts: syntax_contexts
                .iter()
                .map(|ctxt| (*ctxt, data.syntax_context_data[ctxt.0 as usize].key()))
                .collect(),
            expansions: expansions
                .iter()
                .map(|expn| (*expn, data.expn_data(*expn).clone()))
                .collect(),
            syntax_context_order: syntax_contexts,
            expansion_order: expansions,
        });
        let stable_crate_id = LOCAL_CRATE.as_def_id().to_stable_hash_key(hcx).stable_crate_id();
        let structural_expansions = {
            let mut builder = HygieneIdentityBuilder {
                graph: &graph,
                hcx,
                expansion_identity: ExpansionIdentity::Structural,
                syntax_contexts: FxHashMap::default(),
                expansions: FxHashMap::default(),
                visiting_syntax_contexts: FxHashSet::default(),
                visiting_expansions: FxHashSet::default(),
            };
            for &expn in &graph.expansion_order {
                let _ = builder.expansion_identity(expn);
            }
            for &ctxt in &graph.syntax_context_order {
                let _ = builder.syntax_context_identity(ctxt);
            }
            builder.expansions
        };
        let provenance = graph.provenance(roots);

        let mut structural_classes: Vec<_> = graph
            .expansion_order
            .iter()
            .map(|&expn| (structural_expansions[&expn], expn))
            .collect();
        structural_classes.sort_unstable_by_key(|&(identity, _)| identity);
        let mut duplicate_ranks: FxHashMap<_, _> =
            graph.expansion_order.iter().map(|&expn| (expn, None)).collect();
        for class in structural_classes
            .chunk_by_mut(|left, right| left.0 == right.0)
            .filter(|class| class.len() > 1)
        {
            class.sort_unstable_by(|left, right| {
                provenance
                    .get(&left.1)
                    .unwrap_or_else(|| panic!("expansion {:?} has no metadata provenance", left.1))
                    .cmp(provenance.get(&right.1).unwrap_or_else(|| {
                        panic!("expansion {:?} has no metadata provenance", right.1)
                    }))
            });
            assert!(
                class.windows(2).all(|pair| provenance[&pair[0].1] != provenance[&pair[1].1]),
                "structurally identical expansions have indistinguishable metadata provenance"
            );
            for (rank, &(_, expn)) in class.iter().enumerate() {
                let rank =
                    u32::try_from(rank).expect("cannot disambiguate more than U32_MAX expansions");
                *duplicate_ranks
                    .get_mut(&expn)
                    .expect("structural expansion missing from duplicate-rank layout") = Some(rank);
            }
        }

        let mut builder = HygieneIdentityBuilder {
            graph: &graph,
            hcx,
            expansion_identity: ExpansionIdentity::Disambiguated(&duplicate_ranks),
            syntax_contexts: FxHashMap::default(),
            expansions: FxHashMap::default(),
            visiting_syntax_contexts: FxHashSet::default(),
            visiting_expansions: FxHashSet::default(),
        };
        for &expn in &graph.expansion_order {
            let _ = builder.expansion_identity(expn);
        }
        for &ctxt in &graph.syntax_context_order {
            let _ = builder.syntax_context_identity(ctxt);
        }

        let mut ordered_expansions: Vec<_> =
            graph.expansion_order.iter().map(|&id| (builder.expansions[&id], id)).collect();
        ordered_expansions.sort_unstable_by_key(|&(identity, _)| identity);
        assert!(
            ordered_expansions.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "two expansions have the same content-derived metadata identity"
        );
        let mut expansion_hashes = FxHashSet::default();
        let mut expansion_layout = FxHashMap::default();
        expansion_layout.insert(
            ExpnId::root(),
            ArtifactExpansion {
                index: ExpnIndex::ZERO,
                identity: HygieneIdentity(Fingerprint::ZERO),
                hash: ExpnHash(Fingerprint::ZERO),
            },
        );
        for (index, (identity, expn)) in ordered_expansions.into_iter().enumerate() {
            let index =
                u32::try_from(index + 1).expect("cannot encode more than U32_MAX local expansions");
            let hash = ExpnHash::new(stable_crate_id, identity.to_smaller_hash());
            let identity = HygieneIdentity(identity);
            assert!(
                expansion_hashes.insert(hash),
                "two expansions have the same content-derived metadata hash"
            );
            assert!(
                expansion_layout
                    .insert(
                        expn,
                        ArtifactExpansion { index: ExpnIndex::from_u32(index), identity, hash },
                    )
                    .is_none(),
                "duplicate expansion in metadata hygiene layout"
            );
        }

        let mut ordered_syntax_contexts: Vec<_> = graph
            .syntax_context_order
            .iter()
            .map(|&id| (builder.syntax_contexts[&id], id))
            .collect();
        ordered_syntax_contexts.sort_unstable_by_key(|&(identity, _)| identity);
        assert!(
            ordered_syntax_contexts.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "two syntax contexts have the same content-derived metadata identity"
        );
        let mut syntax_context_layout = FxHashMap::default();
        syntax_context_layout.insert(
            SyntaxContext::root(),
            ArtifactSyntaxContext { index: 0, identity: HygieneIdentity(Fingerprint::ZERO) },
        );
        for (index, (identity, ctxt)) in ordered_syntax_contexts.into_iter().enumerate() {
            let index =
                u32::try_from(index + 1).expect("cannot encode more than U32_MAX syntax contexts");
            assert!(
                syntax_context_layout
                    .insert(
                        ctxt,
                        ArtifactSyntaxContext { index, identity: HygieneIdentity(identity) }
                    )
                    .is_none(),
                "duplicate syntax context in metadata hygiene layout"
            );
        }

        HygieneEncodeLayout {
            syntax_contexts: Arc::new(syntax_context_layout),
            expansions: Arc::new(expansion_layout),
        }
    }

    fn syntax_context_identity(&mut self, ctxt: SyntaxContext) -> Fingerprint {
        if ctxt.is_root() {
            return Fingerprint::ZERO;
        }
        if let Some(&identity) = self.syntax_contexts.get(&ctxt) {
            return identity;
        }
        assert!(
            self.visiting_syntax_contexts.insert(ctxt),
            "cycle in metadata syntax-context graph at {ctxt:?}"
        );
        let &(parent, expansion, transparency) =
            self.graph.syntax_contexts.get(&ctxt).unwrap_or_else(|| {
                panic!("metadata hygiene graph omitted syntax context {ctxt:?}")
            });
        let parent = self.syntax_context_identity(parent);
        let expansion = self.expansion_identity(expansion);
        let mut hasher = StableHasher::new();
        0_u8.stable_hash(self, &mut hasher);
        parent.stable_hash(self, &mut hasher);
        expansion.stable_hash(self, &mut hasher);
        transparency.stable_hash(self, &mut hasher);
        let identity = hasher.finish();
        assert!(self.visiting_syntax_contexts.remove(&ctxt));
        self.syntax_contexts.insert(ctxt, identity);
        identity
    }

    fn expansion_identity(&mut self, expn: ExpnId) -> Fingerprint {
        if expn == ExpnId::root() {
            return Fingerprint::ZERO;
        }
        if expn.krate != LOCAL_CRATE {
            return expn.expn_hash().0;
        }
        if let Some(&identity) = self.expansions.get(&expn) {
            return identity;
        }
        assert!(
            self.visiting_expansions.insert(expn),
            "cycle in metadata expansion graph at {expn:?}"
        );
        let data = self
            .graph
            .expansions
            .get(&expn)
            .unwrap_or_else(|| panic!("metadata hygiene graph omitted expansion {expn:?}"))
            .clone();
        let mut hasher = StableHasher::new();
        1_u8.stable_hash(self, &mut hasher);
        data.stable_hash_artifact_semantics(self, &mut hasher);
        if let ExpansionIdentity::Disambiguated(duplicate_ranks) = self.expansion_identity {
            duplicate_ranks[&expn].stable_hash(self, &mut hasher);
        }
        let identity = hasher.finish();
        assert!(self.visiting_expansions.remove(&expn));
        self.expansions.insert(expn, identity);
        identity
    }
}

impl HygieneGraph {
    fn provenance(
        &self,
        roots: UnordMap<HygieneNode, Fingerprint>,
    ) -> FxHashMap<ExpnId, HygieneProvenance> {
        let mut provenance = FxHashMap::default();
        let mut queue = VecDeque::new();
        let mut paths: FxHashMap<HygieneNode, HygieneProvenance> = FxHashMap::default();
        let roots = roots
            .into_items()
            .map(|(node, root)| (root, node))
            .into_sorted_stable_ord_by_key(|(root, _)| root);
        assert!(
            roots.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "two metadata hygiene occurrences have the same provenance hash"
        );
        for (root, node) in roots {
            assert!(
                self.contains(node),
                "metadata hygiene occurrence names a node absent from the reached graph"
            );
            let path = HygieneProvenance { root, edges: Vec::new() };
            if paths.get(&node).is_none_or(|current| &path < current) {
                paths.insert(node, path);
                queue.push_back(node);
            }
        }

        while let Some(node) = queue.pop_front() {
            let path = paths[&node].clone();
            for (edge, dependency) in self.dependencies(node) {
                let mut candidate = path.clone();
                candidate.edges.push(edge);
                if paths.get(&dependency).is_none_or(|current| &candidate < current) {
                    paths.insert(dependency, candidate);
                    queue.push_back(dependency);
                }
            }
        }

        for &expn in &self.expansion_order {
            provenance.insert(
                expn,
                paths
                    .remove(&HygieneNode::Expansion(expn))
                    .unwrap_or_else(|| panic!("expansion {expn:?} has no metadata provenance")),
            );
        }
        assert!(
            self.syntax_contexts.len() == self.syntax_context_order.len()
                && self
                    .syntax_context_order
                    .iter()
                    .all(|ctxt| paths.contains_key(&HygieneNode::SyntaxContext(*ctxt))),
            "a syntax context has no metadata provenance"
        );
        provenance
    }

    fn contains(&self, node: HygieneNode) -> bool {
        match node {
            HygieneNode::SyntaxContext(ctxt) => {
                ctxt.is_root() || self.syntax_contexts.contains_key(&ctxt)
            }
            HygieneNode::Expansion(expn) => {
                expn == ExpnId::root()
                    || expn.krate != LOCAL_CRATE
                    || self.expansions.contains_key(&expn)
            }
        }
    }

    fn dependencies(&self, node: HygieneNode) -> Vec<(HygieneEdge, HygieneNode)> {
        let candidates = match node {
            HygieneNode::SyntaxContext(ctxt) => {
                let &(parent, expn, _) = &self.syntax_contexts[&ctxt];
                vec![
                    (HygieneEdge::ContextParent, HygieneNode::SyntaxContext(parent)),
                    (HygieneEdge::ContextExpansion, HygieneNode::Expansion(expn)),
                ]
            }
            HygieneNode::Expansion(expn) => {
                let data = &self.expansions[&expn];
                vec![
                    (HygieneEdge::ExpansionParent, HygieneNode::Expansion(data.parent)),
                    (
                        HygieneEdge::ExpansionCallSite,
                        HygieneNode::SyntaxContext(data.call_site.ctxt()),
                    ),
                    (
                        HygieneEdge::ExpansionDefSite,
                        HygieneNode::SyntaxContext(data.def_site.ctxt()),
                    ),
                ]
            }
        };
        candidates
            .into_iter()
            .filter(|(_, dependency)| match dependency {
                HygieneNode::SyntaxContext(ctxt) => !ctxt.is_root(),
                HygieneNode::Expansion(expn) => {
                    *expn != ExpnId::root() && expn.krate == LOCAL_CRATE
                }
            })
            .map(|candidate @ (_, dependency)| {
                assert!(
                    self.contains(dependency),
                    "metadata hygiene graph omitted a reached dependency"
                );
                candidate
            })
            .collect()
    }
}

impl<Hcx: StableHashCtxt> StableHashCtxt for HygieneIdentityBuilder<'_, Hcx> {
    fn stable_hash_span(&mut self, raw_span: RawSpan, hasher: &mut StableHasher) {
        self.syntax_context_identity(Span::from_raw_span(raw_span).data_untracked().ctxt)
            .stable_hash(self, hasher);
    }

    fn def_path_hash(&self, def_id: RawDefId) -> RawDefPathHash {
        self.hcx.def_path_hash(def_id)
    }

    fn stable_hash_controls(&self) -> StableHashControls {
        StableHashControls::MetadataHygiene
    }

    fn stable_hash_expn_id(
        &mut self,
        RawExpnId(krate, local_id): RawExpnId,
        ordinary_hash: Fingerprint,
        hasher: &mut StableHasher,
    ) {
        let expn =
            ExpnId { krate: CrateNum::from_u32(krate), local_id: ExpnIndex::from_u32(local_id) };
        let identity =
            if expn.krate == LOCAL_CRATE { self.expansion_identity(expn) } else { ordinary_hash };
        identity.stable_hash(self, hasher);
    }

    fn with_span_hash_mode<R>(
        &mut self,
        mode: SpanHashMode,
        hash: impl FnOnce(&mut Self) -> R,
    ) -> R {
        assert_eq!(
            mode,
            SpanHashMode::Hygiene,
            "content-derived hygiene identity cannot change span hashing modes"
        );
        hash(self)
    }

    fn assert_default_stable_hash_controls(&self, msg: &str) {
        assert_eq!(
            self.stable_hash_controls(),
            StableHashControls::MetadataHygiene,
            "attempted hashing of {msg} outside content-derived hygiene mode"
        );
    }
}

/// Additional information used to assist in decoding hygiene data
#[derive(Default)]
pub struct HygieneDecodeContext {
    // A cache mapping raw serialized per-crate syntax context ids to corresponding decoded
    // `SyntaxContext`s in the current global `HygieneData`.
    remapped_ctxts: Lock<IndexVec<u32, Option<SyntaxContext>>>,
}

/// Register an expansion which has been decoded from the on-disk-cache for the local crate.
pub fn register_local_expn_id(data: ExpnData, hash: ExpnHash) -> ExpnId {
    HygieneData::with(|hygiene_data| {
        let expn_id = hygiene_data.local_expn_data.next_index();
        hygiene_data.local_expn_data.push(Some(data));
        let _eid = hygiene_data.local_expn_hashes.push(hash);
        debug_assert_eq!(expn_id, _eid);

        let expn_id = expn_id.to_expn_id();

        let _old_id = hygiene_data.expn_hash_to_expn_id.insert(hash, expn_id);
        debug_assert!(_old_id.is_none());
        expn_id
    })
}

/// Register an expansion which has been decoded from the metadata of a foreign crate.
pub fn register_expn_id(
    krate: CrateNum,
    local_id: ExpnIndex,
    data: ExpnData,
    hash: ExpnHash,
) -> ExpnId {
    debug_assert!(data.parent == ExpnId::root() || krate == data.parent.krate);
    let expn_id = ExpnId { krate, local_id };
    HygieneData::with(|hygiene_data| {
        let _old_data = hygiene_data.foreign_expn_data.insert(expn_id, data);
        let _old_hash = hygiene_data.foreign_expn_hashes.insert(expn_id, hash);
        debug_assert!(_old_hash.is_none() || _old_hash == Some(hash));
        let _old_id = hygiene_data.expn_hash_to_expn_id.insert(hash, expn_id);
        debug_assert!(_old_id.is_none() || _old_id == Some(expn_id));
    });
    expn_id
}

/// Decode an expansion from the metadata of a foreign crate.
pub fn decode_expn_id(
    krate: CrateNum,
    index: u32,
    decode_data: impl FnOnce(ExpnId) -> (ExpnData, ExpnHash),
) -> ExpnId {
    if index == 0 {
        trace!("decode_expn_id: deserialized root");
        return ExpnId::root();
    }

    let index = ExpnIndex::from_u32(index);

    // This function is used to decode metadata, so it cannot decode information about LOCAL_CRATE.
    debug_assert_ne!(krate, LOCAL_CRATE);
    let expn_id = ExpnId { krate, local_id: index };

    // Fast path if the expansion has already been decoded.
    if HygieneData::with(|hygiene_data| hygiene_data.foreign_expn_data.contains_key(&expn_id)) {
        return expn_id;
    }

    // Don't decode the data inside `HygieneData::with`, since we need to recursively decode
    // other ExpnIds
    let (expn_data, hash) = decode_data(expn_id);

    register_expn_id(krate, index, expn_data, hash)
}

// Decodes `SyntaxContext`, using the provided `HygieneDecodeContext`
// to track which `SyntaxContext`s we have already decoded.
// The provided closure will be invoked to deserialize a `SyntaxContextData`
// if we haven't already seen the id of the `SyntaxContext` we are deserializing.
pub fn decode_syntax_context<D: Decoder>(
    d: &mut D,
    context: &HygieneDecodeContext,
    decode_data: impl FnOnce(&mut D, u32) -> SyntaxContextKey,
) -> SyntaxContext {
    let raw_id: u32 = Decodable::decode(d);
    if raw_id == 0 {
        trace!("decode_syntax_context: deserialized root");
        // The root is special
        return SyntaxContext::root();
    }

    // Look into the cache first.
    // Reminder: `HygieneDecodeContext` is per-crate, so there are no collisions between
    // raw ids from different crate metadatas.
    if let Some(Some(ctxt)) = context.remapped_ctxts.lock().get(raw_id) {
        return *ctxt;
    }

    // Don't try to decode data while holding the lock, since we need to
    // be able to recursively decode a SyntaxContext
    let (parent, expn_id, transparency) = decode_data(d, raw_id);
    let ctxt =
        HygieneData::with(|hygiene_data| hygiene_data.alloc_ctxt(parent, expn_id, transparency));

    context.remapped_ctxts.lock().insert(raw_id, ctxt);

    ctxt
}

impl<E: SpanEncoder> Encodable<E> for LocalExpnId {
    fn encode(&self, e: &mut E) {
        self.to_expn_id().encode(e);
    }
}

impl<D: SpanDecoder> Decodable<D> for LocalExpnId {
    fn decode(d: &mut D) -> Self {
        ExpnId::expect_local(ExpnId::decode(d))
    }
}

pub fn raw_encode_syntax_context(
    ctxt: SyntaxContext,
    context: &HygieneEncodeContext,
    e: &mut impl Encoder,
) {
    if !context.serialized_ctxts.lock().contains(&ctxt) {
        context.latest_ctxts.lock().insert(ctxt);
    }
    context.syntax_context_index(ctxt).encode(e);
}

/// Updates the `disambiguator` field of the corresponding `ExpnData`
/// such that the `Fingerprint` of the `ExpnData` does not collide with
/// any other `ExpnIds`.
///
/// This method is called only when an `ExpnData` is first associated
/// with an `ExpnId` (when the `ExpnId` is initially constructed, or via
/// `set_expn_data`). It is *not* called for foreign `ExpnId`s deserialized
/// from another crate's metadata - since `ExpnHash` includes the stable crate id,
/// collisions are only possible between `ExpnId`s within the same crate.
fn update_disambiguator(expn_data: &mut ExpnData, mut hcx: impl StableHashCtxt) -> ExpnHash {
    // This disambiguator should not have been set yet.
    assert_eq!(expn_data.disambiguator, 0, "Already set disambiguator for ExpnData: {expn_data:?}");
    hcx.assert_default_stable_hash_controls("ExpnData (disambiguator)");
    let mut expn_hash = expn_data.hash_expn(&mut hcx);

    let disambiguator = HygieneData::with(|data| {
        // If this is the first ExpnData with a given hash, then keep our
        // disambiguator at 0 (the default u32 value)
        let disambig = data.expn_data_disambiguators.entry(expn_hash).or_default();
        let disambiguator = *disambig;
        *disambig += 1;
        disambiguator
    });

    if disambiguator != 0 {
        debug!("Set disambiguator for expn_data={:?} expn_hash={:?}", expn_data, expn_hash);

        expn_data.disambiguator = disambiguator;
        expn_hash = expn_data.hash_expn(&mut hcx);

        // Verify that the new disambiguator makes the hash unique
        #[cfg(debug_assertions)]
        HygieneData::with(|data| {
            assert_eq!(
                data.expn_data_disambiguators.get(&expn_hash),
                None,
                "Hash collision after disambiguator update!",
            );
        });
    }

    ExpnHash::new(LOCAL_CRATE.as_def_id().to_stable_hash_key(&mut hcx).stable_crate_id(), expn_hash)
}

impl StableHash for SyntaxContext {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        const TAG_EXPANSION: u8 = 0;
        const TAG_NO_EXPANSION: u8 = 1;

        if self.is_root() {
            TAG_NO_EXPANSION.stable_hash(hcx, hasher);
        } else {
            TAG_EXPANSION.stable_hash(hcx, hasher);
            let (expn_id, transparency) = self.outer_mark();
            expn_id.stable_hash(hcx, hasher);
            transparency.stable_hash(hcx, hasher);
        }
    }
}

impl StableHash for ExpnId {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        hcx.assert_default_stable_hash_controls("ExpnId");
        if *self == ExpnId::root() {
            Fingerprint::ZERO.stable_hash(hcx, hasher);
            return;
        }
        hcx.stable_hash_expn_id(
            RawExpnId(self.krate.as_u32(), self.local_id.as_u32()),
            self.expn_hash().0,
            hasher,
        );
    }
}

impl StableHash for LocalExpnId {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        self.to_expn_id().stable_hash(hcx, hasher);
    }
}

#[cfg(test)]
use crate::create_session_globals_then;

#[test]
fn hygiene_frontiers_form_one_stable_union() {
    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let first = register_local_expn_id(
            ExpnData::default(
                ExpnKind::AstPass(AstPass::TestHarness),
                DUMMY_SP,
                Edition::Edition2024,
                None,
                None,
            ),
            ExpnHash(Fingerprint::new(1, 1)),
        );
        let second = register_local_expn_id(
            ExpnData::default(
                ExpnKind::AstPass(AstPass::ProcMacroHarness),
                DUMMY_SP,
                Edition::Edition2024,
                None,
                None,
            ),
            ExpnHash(Fingerprint::new(2, 2)),
        );
        let context = HygieneEncodeContext::default();
        context.schedule_expn_data_for_encoding(first);
        let mut encoded = Vec::new();
        let hygiene = context.encode_pending(
            &mut (),
            |(), _| {},
            |(), expansion| encoded.push(expansion.id()),
        );
        assert_eq!(encoded, [first]);

        context.schedule_expn_data_for_encoding(first);
        context.schedule_expn_data_for_encoding(second);
        let delta = context.encode_pending(
            &mut (),
            |(), _| {},
            |(), expansion| encoded.push(expansion.id()),
        );
        assert_eq!(encoded, [first, second]);
        let trace = HygieneTrace::new();
        trace.extend(hygiene);
        trace.extend(delta);
        let HygieneTrace { reached, roots: _ } = trace;
        let hygiene = reached.into_inner();

        let layout = HygieneEncodeLayout {
            syntax_contexts: Arc::new(FxHashMap::from_iter([(
                SyntaxContext::root(),
                ArtifactSyntaxContext { index: 0, identity: HygieneIdentity(Fingerprint::ZERO) },
            )])),
            expansions: Arc::new(FxHashMap::from_iter([
                (
                    ExpnId::root(),
                    ArtifactExpansion {
                        index: ExpnIndex::ZERO,
                        identity: HygieneIdentity(Fingerprint::ZERO),
                        hash: ExpnHash(Fingerprint::ZERO),
                    },
                ),
                (
                    first,
                    ArtifactExpansion {
                        index: ExpnIndex::from_u32(1),
                        identity: HygieneIdentity(Fingerprint::new(1, 1)),
                        hash: ExpnHash(Fingerprint::new(1, 1)),
                    },
                ),
                (
                    second,
                    ArtifactExpansion {
                        index: ExpnIndex::from_u32(2),
                        identity: HygieneIdentity(Fingerprint::new(2, 2)),
                        hash: ExpnHash(Fingerprint::new(2, 2)),
                    },
                ),
            ])),
        };
        layout.assert_reached(hygiene);
        assert_ne!(layout.expansion(first).index, layout.expansion(second).index);
    });
}

#[test]
#[should_panic(expected = "metadata emission discovered an unprojected expansion")]
fn closed_hygiene_layout_rejects_an_untraced_expansion() {
    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let expansion = register_local_expn_id(
            ExpnData::default(
                ExpnKind::AstPass(AstPass::TestHarness),
                DUMMY_SP,
                Edition::Edition2024,
                None,
                None,
            ),
            ExpnHash(Fingerprint::new(1, 1)),
        );
        let context = HygieneEncodeContext::with_layout(HygieneEncodeLayout {
            syntax_contexts: Arc::new(FxHashMap::from_iter([(
                SyntaxContext::root(),
                ArtifactSyntaxContext { index: 0, identity: HygieneIdentity(Fingerprint::ZERO) },
            )])),
            expansions: Arc::new(FxHashMap::from_iter([(
                ExpnId::root(),
                ArtifactExpansion {
                    index: ExpnIndex::ZERO,
                    identity: HygieneIdentity(Fingerprint::ZERO),
                    hash: ExpnHash(Fingerprint::ZERO),
                },
            )])),
        });

        context.expansion_index(expansion);
    });
}

#[test]
#[should_panic(expected = "cycle in metadata syntax-context graph")]
fn content_identity_rejects_a_context_only_cycle_before_provenance() {
    struct TestHashContext;

    impl StableHashCtxt for TestHashContext {
        fn stable_hash_span(&mut self, _: RawSpan, _: &mut StableHasher) {
            unreachable!()
        }

        fn def_path_hash(&self, _: RawDefId) -> RawDefPathHash {
            RawDefPathHash([0; 16])
        }

        fn stable_hash_controls(&self) -> StableHashControls {
            StableHashControls::MetadataHygiene
        }

        fn stable_hash_expn_id(&mut self, _: RawExpnId, _: Fingerprint, _: &mut StableHasher) {
            unreachable!()
        }

        fn with_span_hash_mode<R>(
            &mut self,
            _: SpanHashMode,
            hash: impl FnOnce(&mut Self) -> R,
        ) -> R {
            hash(self)
        }

        fn assert_default_stable_hash_controls(&self, _: &str) {}
    }

    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let ctxt = HygieneData::with(|data| {
            let ctxt =
                data.alloc_ctxt(SyntaxContext::root(), ExpnId::root(), Transparency::Transparent);
            data.syntax_context_data[ctxt.0 as usize].parent = ctxt;
            ctxt
        });
        let mut syntax_contexts = UnordSet::default();
        syntax_contexts.insert(ctxt);
        HygieneIdentityBuilder::build(
            &mut TestHashContext,
            HygieneDelta { syntax_contexts, expansions: UnordSet::default() },
            UnordMap::from_iter([(HygieneNode::SyntaxContext(ctxt), Fingerprint::new(1, 1))]),
        );
    });
}

#[test]
fn artifact_payloads_follow_artifact_order() {
    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let first = register_local_expn_id(
            ExpnData::default(
                ExpnKind::AstPass(AstPass::TestHarness),
                DUMMY_SP,
                Edition::Edition2024,
                None,
                None,
            ),
            ExpnHash(Fingerprint::new(9, 9)),
        );
        let second = register_local_expn_id(
            ExpnData::default(
                ExpnKind::AstPass(AstPass::ProcMacroHarness),
                DUMMY_SP,
                Edition::Edition2024,
                None,
                None,
            ),
            ExpnHash(Fingerprint::new(1, 1)),
        );
        let first_context = SyntaxContext::root().apply_mark(first, Transparency::Transparent);
        let second_context = SyntaxContext::root().apply_mark(second, Transparency::Transparent);
        let context = HygieneEncodeContext::with_layout(HygieneEncodeLayout {
            syntax_contexts: Arc::new(FxHashMap::from_iter([
                (
                    SyntaxContext::root(),
                    ArtifactSyntaxContext {
                        index: 0,
                        identity: HygieneIdentity(Fingerprint::ZERO),
                    },
                ),
                (
                    first_context,
                    ArtifactSyntaxContext {
                        index: 1,
                        identity: HygieneIdentity(Fingerprint::new(1, 1)),
                    },
                ),
                (
                    second_context,
                    ArtifactSyntaxContext {
                        index: 2,
                        identity: HygieneIdentity(Fingerprint::new(2, 2)),
                    },
                ),
            ])),
            expansions: Arc::new(FxHashMap::from_iter([
                (
                    ExpnId::root(),
                    ArtifactExpansion {
                        index: ExpnIndex::ZERO,
                        identity: HygieneIdentity(Fingerprint::ZERO),
                        hash: ExpnHash(Fingerprint::ZERO),
                    },
                ),
                (
                    first,
                    ArtifactExpansion {
                        index: ExpnIndex::from_u32(1),
                        identity: HygieneIdentity(Fingerprint::new(1, 1)),
                        hash: ExpnHash(Fingerprint::new(1, 1)),
                    },
                ),
                (
                    second,
                    ArtifactExpansion {
                        index: ExpnIndex::from_u32(2),
                        identity: HygieneIdentity(Fingerprint::new(2, 2)),
                        hash: ExpnHash(Fingerprint::new(2, 2)),
                    },
                ),
            ])),
        });
        let mut sink = rustc_serialize::opaque::mem_encoder::MemEncoder::new();
        raw_encode_syntax_context(second_context, &context, &mut sink);
        raw_encode_syntax_context(first_context, &context, &mut sink);
        context.schedule_expn_data_for_encoding(second);
        context.schedule_expn_data_for_encoding(first);
        let mut encoded = (Vec::new(), Vec::new());
        context.encode_pending(
            &mut encoded,
            |encoded, context| encoded.0.push(context.index()),
            |encoded, expansion| encoded.1.push(expansion.index().as_u32()),
        );

        assert_eq!(encoded.0, [1, 2]);
        assert_eq!(encoded.1, [1, 2]);
    });
}
