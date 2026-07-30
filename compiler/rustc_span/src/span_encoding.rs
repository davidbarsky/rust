use std::hash::Hash;

use rustc_data_structures::fx::FxIndexSet;
use rustc_data_structures::stable_hash::{
    RawSpan, SpanHashMode, StableHash, StableHashControls, StableHashCtxt, StableHasher,
};
use rustc_serialize::Encodable;
// This code is very hot and uses lots of arithmetic, avoid overflow checks for performance.
// See https://github.com/rust-lang/rust/pull/119440#issuecomment-1874255727
use rustc_serialize::int_overflow::DebugStrictAdd;

use crate::def_id::{DefIndex, LocalDefId};
use crate::hygiene::SyntaxContext;
use crate::{
    BytePos, EXTERNAL_SPAN_DATA, ExternalSpanData, ExternalSpanId, ExternalSpanSlot, SPAN_TRACK,
    SpanData,
};

/// A compressed span.
///
/// [`SpanData`] is 16 bytes, which is too big to stick everywhere. `Span` only
/// takes up 8 bytes, with less space for the length, parent and context. The
/// vast majority (99.9%+) of `SpanData` instances can be made to fit within
/// those 8 bytes. Any `SpanData` whose fields don't fit into a `Span` are
/// stored in a separate interner table, and the `Span` will index into that
/// table. Interning is rare enough that the cost is low, but common enough
/// that the code is exercised regularly.
///
/// An earlier version of this code used only 4 bytes for `Span`, but that was
/// slower because only 80--90% of spans could be stored inline (even less in
/// very large crates) and so the interner was used a lot more. That version of
/// the code also predated the storage of parents.
///
/// There are five different span forms.
///
/// Inline-context format (requires non-huge length, non-huge context, and no parent):
/// - `span.lo_or_index == span_data.lo`
/// - `span.len_with_tag_or_marker == len == span_data.hi - span_data.lo` (must be `<= MAX_LEN`)
/// - `span.ctxt_or_parent_or_marker == span_data.ctxt` (must be `<= MAX_CTXT`)
///
/// Inline-parent format (requires non-huge length, root context, and non-huge parent):
/// - `span.lo_or_index == span_data.lo`
/// - `span.len_with_tag_or_marker & !PARENT_TAG == len == span_data.hi - span_data.lo`
///   (must be `<= MAX_LEN`)
/// - `span.len_with_tag_or_marker` has top bit (`PARENT_TAG`) set
/// - `span.ctxt_or_parent_or_marker == span_data.parent` (must be `<= MAX_CTXT`)
///
/// Partially-interned format (requires non-huge context):
/// - `span.lo_or_index == index` (indexes into the interner table)
/// - `span.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER`
/// - `span.ctxt_or_parent_or_marker == span_data.ctxt` (must be `<= MAX_CTXT`)
///
/// Fully-interned format (all cases not covered above):
/// - `span.lo_or_index == index` (indexes into the interner table)
/// - `span.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER`
/// - `span.ctxt_or_parent_or_marker == CTXT_INTERNED_MARKER`
///
/// External format (source coordinates decoded from another crate's metadata):
/// - `span.lo_or_index == index` (indexes into the external span interner table)
/// - `span.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER`
/// - `span.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER`
///
/// The partially-interned form requires looking in the interning table for
/// lo and length, but the context is stored inline as well as interned.
/// This is useful because context lookups are often done in isolation, and
/// inline lookups are quicker.
///
/// The external form keeps the syntax context available without loading source
/// coordinates. Operations that need `lo` or `hi` resolve the external span
/// through `EXTERNAL_SPAN_DATA`, which lets the query engine track the spans
/// cache only for computations that observe positions.
///
/// Notes about the choice of field sizes:
/// - `lo` is 32 bits in both `Span` and `SpanData`, which means that `lo`
///   values never cause interning. The number of bits needed for `lo`
///   depends on the crate size. 32 bits allows up to 4 GiB of code in a crate.
///   Having no compression on this field means there is no performance cliff
///   if a crate exceeds a particular size.
/// - `len` is ~15 bits in `Span` (a u16, minus 1 bit for PARENT_TAG) and 32
///   bits in `SpanData`, which means that large `len` values will cause
///   interning. The number of bits needed for `len` does not depend on the
///   crate size. The most common numbers of bits for `len` are from 0 to 7,
///   with a peak usually at 3 or 4, and then it drops off quickly from 8
///   onwards. 15 bits is enough for 99.99%+ of cases, but larger values
///   (sometimes 20+ bits) might occur dozens of times in a typical crate.
/// - `ctxt_or_parent_or_marker` is 16 bits in `Span` and two 32 bit fields in
///   `SpanData`, which means intering will happen if `ctxt` is large, if
///   `parent` is large, or if both values are non-zero. The number of bits
///   needed for `ctxt` values depend partly on the crate size and partly on
///   the form of the code. No crates in `rustc-perf` need more than 15 bits
///   for `ctxt_or_parent_or_marker`, but larger crates might need more than 16
///   bits. The number of bits needed for `parent` hasn't been measured,
///   because `parent` isn't currently used by default.
///
/// In order to reliably use parented and external spans in incremental compilation,
/// accesses to `lo` and `hi` must introduce dependencies on their source coordinates.
/// `SPAN_TRACK` and `EXTERNAL_SPAN_DATA` provide access to the query engine without
/// making `rustc_span` depend on it.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[rustc_pass_by_value]
pub struct Span {
    lo_or_index: u32,
    len_with_tag_or_marker: u16,
    ctxt_or_parent_or_marker: u16,
}

// Convenience structures for all span formats.
#[derive(Clone, Copy)]
struct InlineCtxt {
    lo: u32,
    len: u16,
    ctxt: u16,
}

#[derive(Clone, Copy)]
struct InlineParent {
    lo: u32,
    len_with_tag: u16,
    parent: u16,
}

#[derive(Clone, Copy)]
struct PartiallyInterned {
    index: u32,
    ctxt: u16,
}

#[derive(Clone, Copy)]
struct Interned {
    index: u32,
}

#[derive(Clone, Copy)]
struct External {
    index: u32,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ExternalSpan {
    cnum: crate::def_id::CrateNum,
    lo: ExternalSpanSlot,
    hi: ExternalSpanSlot,
    ctxt: SyntaxContext,
}

#[derive(Clone, Copy)]
enum IndirectCtxt {
    Span(usize),
    External(usize),
}

impl InlineCtxt {
    #[inline]
    fn data(self) -> SpanData {
        let len = self.len as u32;
        debug_assert!(len <= MAX_LEN);
        SpanData {
            lo: BytePos(self.lo),
            hi: BytePos(self.lo.debug_strict_add(len)),
            ctxt: SyntaxContext::from_u16(self.ctxt),
            parent: None,
        }
    }
    #[inline]
    fn span(lo: u32, len: u16, ctxt: u16) -> Span {
        Span { lo_or_index: lo, len_with_tag_or_marker: len, ctxt_or_parent_or_marker: ctxt }
    }
    #[inline]
    fn from_span(span: Span) -> InlineCtxt {
        let (lo, len, ctxt) =
            (span.lo_or_index, span.len_with_tag_or_marker, span.ctxt_or_parent_or_marker);
        InlineCtxt { lo, len, ctxt }
    }
}

impl InlineParent {
    #[inline]
    fn data(self) -> SpanData {
        let len = (self.len_with_tag & !PARENT_TAG) as u32;
        debug_assert!(len <= MAX_LEN);
        SpanData {
            lo: BytePos(self.lo),
            hi: BytePos(self.lo.debug_strict_add(len)),
            ctxt: SyntaxContext::root(),
            parent: Some(LocalDefId { local_def_index: DefIndex::from_u16(self.parent) }),
        }
    }
    #[inline]
    fn span(lo: u32, len: u16, parent: u16) -> Span {
        let (lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker) =
            (lo, PARENT_TAG | len, parent);
        Span { lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker }
    }
    #[inline]
    fn from_span(span: Span) -> InlineParent {
        let (lo, len_with_tag, parent) =
            (span.lo_or_index, span.len_with_tag_or_marker, span.ctxt_or_parent_or_marker);
        InlineParent { lo, len_with_tag, parent }
    }
}

impl PartiallyInterned {
    #[inline]
    fn data(self) -> SpanData {
        SpanData {
            ctxt: SyntaxContext::from_u16(self.ctxt),
            ..with_span_interner(|interner| interner.spans[self.index as usize])
        }
    }
    #[inline]
    fn span(index: u32, ctxt: u16) -> Span {
        let (lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker) =
            (index, BASE_LEN_INTERNED_MARKER, ctxt);
        Span { lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker }
    }
    #[inline]
    fn from_span(span: Span) -> PartiallyInterned {
        PartiallyInterned { index: span.lo_or_index, ctxt: span.ctxt_or_parent_or_marker }
    }
}

impl Interned {
    #[inline]
    fn data(self) -> SpanData {
        with_span_interner(|interner| interner.spans[self.index as usize])
    }
    #[inline]
    fn span(index: u32) -> Span {
        let (lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker) =
            (index, BASE_LEN_INTERNED_MARKER, CTXT_INTERNED_MARKER);
        Span { lo_or_index, len_with_tag_or_marker, ctxt_or_parent_or_marker }
    }
    #[inline]
    fn from_span(span: Span) -> Interned {
        Interned { index: span.lo_or_index }
    }
}

impl External {
    #[inline]
    fn data(self) -> SpanData {
        let span = with_span_interner(|interner| interner.external_spans[self.index as usize]);
        let id = |slot| ExternalSpanId { cnum: span.cnum, slot };
        let ExternalSpanData { mut lo, hi: same_span_hi } = (*EXTERNAL_SPAN_DATA)(id(span.lo));
        let mut hi =
            if span.lo == span.hi { same_span_hi } else { (*EXTERNAL_SPAN_DATA)(id(span.hi)).hi };
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        SpanData { lo, hi, ctxt: span.ctxt, parent: None }
    }
}

// This code is very hot, and converting span to an enum and matching on it doesn't optimize away
// properly. So we are using a macro emulating such a match, but expand it directly to an if-else
// chain.
macro_rules! match_span_kind {
    (
        $span:expr,
        InlineCtxt($span1:ident) => $arm1:expr,
        InlineParent($span2:ident) => $arm2:expr,
        PartiallyInterned($span3:ident) => $arm3:expr,
        Interned($span4:ident) => $arm4:expr,
        External($span5:ident) => $arm5:expr,
    ) => {
        if $span.len_with_tag_or_marker != BASE_LEN_INTERNED_MARKER {
            if $span.len_with_tag_or_marker & PARENT_TAG == 0 {
                // Inline-context format.
                let $span1 = InlineCtxt::from_span($span);
                $arm1
            } else {
                // Inline-parent format.
                let $span2 = InlineParent::from_span($span);
                $arm2
            }
        } else if $span.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER {
            // External format.
            let $span5 = External { index: $span.lo_or_index };
            $arm5
        } else if $span.ctxt_or_parent_or_marker != CTXT_INTERNED_MARKER {
            // Partially-interned format.
            let $span3 = PartiallyInterned::from_span($span);
            $arm3
        } else {
            // Interned format.
            let $span4 = Interned::from_span($span);
            $arm4
        }
    };
}

// `MAX_LEN` is chosen so that `PARENT_TAG | MAX_LEN` is distinct from
// `BASE_LEN_INTERNED_MARKER`. (If `MAX_LEN` was 1 higher, this wouldn't be true.)
const MAX_LEN: u32 = 0b0111_1111_1111_1110;
const MAX_CTXT: u32 = 0b0111_1111_1111_1110;
const PARENT_TAG: u16 = 0b1000_0000_0000_0000;
const BASE_LEN_INTERNED_MARKER: u16 = 0b1111_1111_1111_1111;
const CTXT_EXTERNAL_MARKER: u16 = 0b1111_1111_1111_1110;
const CTXT_INTERNED_MARKER: u16 = 0b1111_1111_1111_1111;

/// The dummy span has zero position, length, and context, and no parent.
pub const DUMMY_SP: Span =
    Span { lo_or_index: 0, len_with_tag_or_marker: 0, ctxt_or_parent_or_marker: 0 };

impl Span {
    #[inline]
    pub fn new(
        mut lo: BytePos,
        mut hi: BytePos,
        ctxt: SyntaxContext,
        parent: Option<LocalDefId>,
    ) -> Self {
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }

        // Small len and ctxt may enable one of fully inline formats (or may not).
        let (len, ctxt32) = (hi.0 - lo.0, ctxt.as_u32());
        if len <= MAX_LEN && ctxt32 <= MAX_CTXT {
            match parent {
                None => return InlineCtxt::span(lo.0, len as u16, ctxt32 as u16),
                Some(parent) => {
                    let parent32 = parent.local_def_index.as_u32();
                    if ctxt32 == 0 && parent32 <= MAX_CTXT {
                        return InlineParent::span(lo.0, len as u16, parent32 as u16);
                    }
                }
            }
        }

        // Otherwise small ctxt may enable the partially inline format.
        let index = |ctxt| {
            with_span_interner(|interner| interner.intern(&SpanData { lo, hi, ctxt, parent }))
        };
        if ctxt32 <= MAX_CTXT {
            // Interned ctxt should never be read, so it can use any value.
            PartiallyInterned::span(index(SyntaxContext::from_u32(u32::MAX)), ctxt32 as u16)
        } else {
            Interned::span(index(ctxt))
        }
    }

    /// Defers loading an external span's source coordinates while retaining its hygiene context.
    #[inline]
    pub fn new_external(id: ExternalSpanId, ctxt: SyntaxContext) -> Self {
        Self::new_external_with_slots(id.cnum, id.slot, id.slot, ctxt)
    }

    /// Defers source coordinates whose endpoints occupy distinct slots in one external artifact.
    ///
    /// Owning the crate number once prevents a composed span from addressing unrelated artifacts.
    #[inline]
    pub fn new_external_with_slots(
        cnum: crate::def_id::CrateNum,
        lo: ExternalSpanSlot,
        hi: ExternalSpanSlot,
        ctxt: SyntaxContext,
    ) -> Self {
        let index = with_span_interner(|interner| {
            let (index, _) =
                interner.external_spans.insert_full(ExternalSpan { cnum, lo, hi, ctxt });
            index as u32
        });
        Span {
            lo_or_index: index,
            len_with_tag_or_marker: BASE_LEN_INTERNED_MARKER,
            ctxt_or_parent_or_marker: CTXT_EXTERNAL_MARKER,
        }
    }

    #[inline]
    pub fn data(self) -> SpanData {
        let data = match_span_kind! {
            self,
            InlineCtxt(span) => span.data(),
            InlineParent(span) => span.data(),
            PartiallyInterned(span) => span.data(),
            Interned(span) => span.data(),
            External(span) => span.data(),
        };
        if let Some(parent) = data.parent {
            (*SPAN_TRACK)(parent);
        }
        data
    }

    /// Internal function to translate between an encoded span and the expanded representation.
    /// This function must not be used outside the incremental engine.
    #[inline]
    pub fn data_untracked(self) -> SpanData {
        match_span_kind! {
            self,
            InlineCtxt(span) => span.data(),
            InlineParent(span) => span.data(),
            PartiallyInterned(span) => span.data(),
            Interned(span) => span.data(),
            External(span) => span.data(),
        }
    }

    /// Returns `true` if this span comes from any kind of macro, desugaring or inlining.
    #[inline]
    pub fn from_expansion(self) -> bool {
        let ctxt = match_span_kind! {
            self,
            // All branches here, except `InlineParent`, actually return `span.ctxt_or_parent_or_marker`.
            // Since `Interned` is selected if the field contains `CTXT_INTERNED_MARKER` returning that value
            // as the context allows the compiler to optimize out the branch that selects between either
            // `Interned` and `PartiallyInterned`.
            //
            // Interned contexts can never be the root context and `CTXT_INTERNED_MARKER` has a different value
            // than the root context so this works for checking is this is an expansion.
            InlineCtxt(span) => SyntaxContext::from_u16(span.ctxt),
            InlineParent(_span) => SyntaxContext::root(),
            PartiallyInterned(span) => SyntaxContext::from_u16(span.ctxt),
            Interned(_span) => SyntaxContext::from_u16(CTXT_INTERNED_MARKER),
            External(span) => {
                with_span_interner(|interner| interner.external_spans[span.index as usize].ctxt)
            },
        };
        !ctxt.is_root()
    }

    /// Returns `true` if this is a dummy span with any hygienic context.
    #[inline]
    pub fn is_dummy(self) -> bool {
        if self.len_with_tag_or_marker != BASE_LEN_INTERNED_MARKER {
            // Inline-context or inline-parent format.
            let lo = self.lo_or_index;
            let len = (self.len_with_tag_or_marker & !PARENT_TAG) as u32;
            debug_assert!(len <= MAX_LEN);
            lo == 0 && len == 0
        } else {
            // Fully-interned or partially-interned format.
            if self.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER {
                false
            } else {
                let index = self.lo_or_index;
                let data = with_span_interner(|interner| interner.spans[index as usize]);
                data.lo == BytePos(0) && data.hi == BytePos(0)
            }
        }
    }

    #[inline]
    pub fn map_ctxt(self, map: impl FnOnce(SyntaxContext) -> SyntaxContext) -> Span {
        let data = match_span_kind! {
            self,
            InlineCtxt(span) => {
                // This format occurs 1-2 orders of magnitude more often than others (#125017),
                // so it makes sense to micro-optimize it to avoid `span.data()` and `Span::new()`.
                let new_ctxt = map(SyntaxContext::from_u16(span.ctxt));
                let new_ctxt32 = new_ctxt.as_u32();
                return if new_ctxt32 <= MAX_CTXT {
                    // Any small new context including zero will preserve the format.
                    InlineCtxt::span(span.lo, span.len, new_ctxt32 as u16)
                } else {
                    span.data().with_ctxt(new_ctxt)
                };
            },
            InlineParent(span) => span.data(),
            PartiallyInterned(span) => span.data(),
            Interned(span) => span.data(),
            External(span) => {
                let external = with_span_interner(|interner| {
                    interner.external_spans[span.index as usize]
                });
                return Span::new_external_with_slots(
                    external.cnum,
                    external.lo,
                    external.hi,
                    map(external.ctxt),
                );
            },
        };

        data.with_ctxt(map(data.ctxt))
    }

    /// Replaces the lower endpoint without resolving deferred external coordinates.
    ///
    /// When both spans are external, the result retains their artifact-local endpoints so
    /// position-independent compiler work remains independent of their source coordinates.
    #[inline]
    pub fn with_lo_from(self, other: Span) -> Span {
        if self.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && self.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
            && other.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && other.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
        {
            let (this, other) = with_span_interner(|interner| {
                (
                    interner.external_spans[self.lo_or_index as usize],
                    interner.external_spans[other.lo_or_index as usize],
                )
            });
            if this.cnum == other.cnum {
                return Span::new_external_with_slots(this.cnum, other.lo, this.hi, this.ctxt);
            }
        }
        self.with_lo(other.lo())
    }

    /// Replaces the upper endpoint without resolving deferred external coordinates.
    ///
    /// When both spans are external, the result retains their artifact-local endpoints so
    /// position-independent compiler work remains independent of their source coordinates.
    #[inline]
    pub fn with_hi_from(self, other: Span) -> Span {
        if self.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && self.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
            && other.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && other.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
        {
            let (this, other) = with_span_interner(|interner| {
                (
                    interner.external_spans[self.lo_or_index as usize],
                    interner.external_spans[other.lo_or_index as usize],
                )
            });
            if this.cnum == other.cnum {
                return Span::new_external_with_slots(this.cnum, this.lo, other.hi, this.ctxt);
            }
        }
        self.with_hi(other.hi())
    }

    // Returns either syntactic context, if it can be retrieved without taking the interner lock,
    // or an index into the interner if it cannot.
    #[inline]
    fn inline_ctxt(self) -> Result<SyntaxContext, IndirectCtxt> {
        match_span_kind! {
            self,
            InlineCtxt(span) => Ok(SyntaxContext::from_u16(span.ctxt)),
            InlineParent(_span) => Ok(SyntaxContext::root()),
            PartiallyInterned(span) => Ok(SyntaxContext::from_u16(span.ctxt)),
            Interned(span) => Err(IndirectCtxt::Span(span.index as usize)),
            External(span) => Err(IndirectCtxt::External(span.index as usize)),
        }
    }

    /// This function is used as a fast path when decoding the full `SpanData` is not necessary.
    /// It's a cut-down version of `data_untracked`.
    #[cfg_attr(not(test), rustc_diagnostic_item = "SpanCtxt")]
    #[inline]
    pub fn ctxt(self) -> SyntaxContext {
        self.inline_ctxt().unwrap_or_else(|index| {
            with_span_interner(|interner| match index {
                IndirectCtxt::Span(index) => interner.spans[index].ctxt,
                IndirectCtxt::External(index) => interner.external_spans[index].ctxt,
            })
        })
    }

    #[inline]
    pub fn eq_ctxt(self, other: Span) -> bool {
        match (self.inline_ctxt(), other.inline_ctxt()) {
            (Ok(ctxt1), Ok(ctxt2)) => ctxt1 == ctxt2,
            (Ok(ctxt), Err(IndirectCtxt::External(index)))
            | (Err(IndirectCtxt::External(index)), Ok(ctxt)) => {
                with_span_interner(|interner| ctxt == interner.external_spans[index].ctxt)
            }
            (Ok(_ctxt), Err(IndirectCtxt::Span(_index)))
            | (Err(IndirectCtxt::Span(_index)), Ok(_ctxt)) => false,
            (Err(index1), Err(index2)) => with_span_interner(|interner| {
                let ctxt = |index| match index {
                    IndirectCtxt::Span(index) => interner.spans[index].ctxt,
                    IndirectCtxt::External(index) => interner.external_spans[index].ctxt,
                };
                ctxt(index1) == ctxt(index2)
            }),
        }
    }

    #[inline]
    pub fn with_parent(self, parent: Option<LocalDefId>) -> Span {
        let data = match_span_kind! {
            self,
            InlineCtxt(span) => {
                // This format occurs 1-2 orders of magnitude more often than others (#126544),
                // so it makes sense to micro-optimize it to avoid `span.data()` and `Span::new()`.
                // Copypaste from `Span::new`, the small len & ctxt conditions are known to hold.
                match parent {
                    None => return self,
                    Some(parent) => {
                        let parent32 = parent.local_def_index.as_u32();
                        if span.ctxt == 0 && parent32 <= MAX_CTXT {
                            return InlineParent::span(span.lo, span.len, parent32 as u16);
                        }
                    }
                }
                span.data()
            },
            InlineParent(span) => span.data(),
            PartiallyInterned(span) => span.data(),
            Interned(span) => span.data(),
            External(span) => match parent {
                None => return self,
                Some(_) => span.data(),
            },
        };

        if let Some(old_parent) = data.parent {
            (*SPAN_TRACK)(old_parent);
        }
        data.with_parent(parent)
    }

    #[inline]
    pub fn parent(self) -> Option<LocalDefId> {
        let interned_parent =
            |index: u32| with_span_interner(|interner| interner.spans[index as usize].parent);
        match_span_kind! {
            self,
            InlineCtxt(_span) => None,
            InlineParent(span) => Some(LocalDefId { local_def_index: DefIndex::from_u16(span.parent) }),
            PartiallyInterned(span) => interned_parent(span.index),
            Interned(span) => interned_parent(span.index),
            External(_span) => None,
        }
    }

    #[inline]
    pub(crate) fn to_raw_span(self) -> RawSpan {
        // Field order must match `from_raw_span`.
        RawSpan(self.lo_or_index, self.len_with_tag_or_marker, self.ctxt_or_parent_or_marker)
    }

    #[inline]
    pub fn from_raw_span(RawSpan(a, b, c): RawSpan) -> Span {
        // Field order must match `to_raw_span`.
        Span { lo_or_index: a, len_with_tag_or_marker: b, ctxt_or_parent_or_marker: c }
    }
}

impl<E: crate::SpanEncoder> Encodable<E> for Span {
    fn encode(&self, encoder: &mut E) {
        if self.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && self.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
        {
            let external =
                with_span_interner(|interner| interner.external_spans[self.lo_or_index as usize]);
            encoder.encode_external_span(external.cnum, external.lo, external.hi, external.ctxt);
        } else {
            encoder.encode_span(*self);
        }
    }
}

impl StableHash for Span {
    fn stable_hash<Hcx: StableHashCtxt>(&self, hcx: &mut Hcx, hasher: &mut StableHasher) {
        if self.len_with_tag_or_marker == BASE_LEN_INTERNED_MARKER
            && self.ctxt_or_parent_or_marker == CTXT_EXTERNAL_MARKER
        {
            let external =
                with_span_interner(|interner| interner.external_spans[self.lo_or_index as usize]);
            match hcx.stable_hash_controls() {
                StableHashControls::Span(SpanHashMode::Ignore) => {}
                StableHashControls::Span(SpanHashMode::Hygiene)
                | StableHashControls::MetadataHygiene => {
                    external.ctxt.stable_hash(hcx, hasher);
                    Option::<LocalDefId>::None.stable_hash(hcx, hasher);
                }
                StableHashControls::Span(SpanHashMode::Full) => {
                    external.ctxt.stable_hash(hcx, hasher);
                    Option::<LocalDefId>::None.stable_hash(hcx, hasher);
                    Hash::hash(&3u8, hasher);
                    external.cnum.stable_hash(hcx, hasher);
                    external.lo.stable_hash(hcx, hasher);
                    external.hi.stable_hash(hcx, hasher);
                }
            }
            return;
        }

        hcx.stable_hash_span(self.to_raw_span(), hasher)
    }
}

#[derive(Default)]
pub(crate) struct SpanInterner {
    spans: FxIndexSet<SpanData>,
    external_spans: FxIndexSet<ExternalSpan>,
}

impl SpanInterner {
    fn intern(&mut self, span_data: &SpanData) -> u32 {
        let (index, _) = self.spans.insert_full(*span_data);
        index as u32
    }
}

// If an interner exists, return it. Otherwise, prepare a fresh one.
#[inline]
fn with_span_interner<T, F: FnOnce(&mut SpanInterner) -> T>(f: F) -> T {
    crate::with_session_globals(|session_globals| f(&mut session_globals.span_interner.lock()))
}
