use rustc_data_structures::fx::FxHashSet;

use super::*;

#[test]
fn external_span_preserves_context_equality() {
    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let ctxt = SyntaxContext::root();
        let inline = Span::new(BytePos(1), BytePos(2), ctxt, None);
        let external = Span::new_external(
            ExternalSpanId { cnum: LOCAL_CRATE, slot: ExternalSpanSlot::from_u32(0) },
            ctxt,
        );

        assert!(inline.eq_ctxt(external));
        assert!(external.eq_ctxt(inline));
        assert!(!external.is_dummy());

        let other_external = Span::new_external(
            ExternalSpanId { cnum: LOCAL_CRATE, slot: ExternalSpanSlot::from_u32(1) },
            ctxt,
        );
        assert!(external.with_lo_from(other_external).eq_ctxt(inline));
        assert!(external.with_hi_from(other_external).eq_ctxt(inline));

        let name = sym::field;
        let mut identifiers = FxHashSet::default();
        identifiers.insert(Ident::new(name, inline));
        assert!(identifiers.contains(&Ident::new(name, external)));
    });
}

#[test]
fn external_span_normalizes_composed_endpoints() {
    fn external_span_data(id: ExternalSpanId) -> ExternalSpanData {
        match id.slot.as_u32() {
            0 => ExternalSpanData { lo: BytePos(10), hi: BytePos(20) },
            1 => ExternalSpanData { lo: BytePos(30), hi: BytePos(40) },
            slot => panic!("unexpected external span slot {slot}"),
        }
    }

    let previous =
        EXTERNAL_SPAN_DATA.swap(&(external_span_data as fn(ExternalSpanId) -> ExternalSpanData));
    let _restore = rustc_data_structures::defer(move || {
        EXTERNAL_SPAN_DATA.swap(previous);
    });

    create_session_globals_then(Edition::Edition2024, &[], None, || {
        let earlier = Span::new_external(
            ExternalSpanId { cnum: LOCAL_CRATE, slot: ExternalSpanSlot::from_u32(0) },
            SyntaxContext::root(),
        );
        let later = Span::new_external(
            ExternalSpanId { cnum: LOCAL_CRATE, slot: ExternalSpanSlot::from_u32(1) },
            SyntaxContext::root(),
        );
        let expected = Span::new(BytePos(20), BytePos(30), SyntaxContext::root(), None).data();

        assert_eq!(earlier.with_lo_from(later).data(), expected);
        assert_eq!(later.with_hi_from(earlier).data(), expected);
    });
}

#[test]
fn test_lookup_line() {
    let source = "abcdefghijklm\nabcdefghij\n...".to_owned();
    let mut sf = SourceFile::new(
        FileName::Anon(Hash64::ZERO),
        source,
        SourceFileHashAlgorithm::Sha256,
        Some(SourceFileHashAlgorithm::Sha256),
    )
    .unwrap();
    sf.start_pos = BytePos(3);
    assert_eq!(sf.lines(), &[RelativeBytePos(0), RelativeBytePos(14), RelativeBytePos(25)]);

    assert_eq!(sf.lookup_line(RelativeBytePos(0)), Some(0));
    assert_eq!(sf.lookup_line(RelativeBytePos(1)), Some(0));

    assert_eq!(sf.lookup_line(RelativeBytePos(13)), Some(0));
    assert_eq!(sf.lookup_line(RelativeBytePos(14)), Some(1));
    assert_eq!(sf.lookup_line(RelativeBytePos(15)), Some(1));

    assert_eq!(sf.lookup_line(RelativeBytePos(25)), Some(2));
    assert_eq!(sf.lookup_line(RelativeBytePos(26)), Some(2));
}

#[test]
fn test_normalize_newlines() {
    fn check(before: &str, after: &str, expected_positions: &[u32]) {
        let mut actual = before.to_string();
        let mut actual_positions = vec![];
        normalize_newlines(&mut actual, &mut actual_positions);
        let actual_positions: Vec<_> = actual_positions.into_iter().map(|nc| nc.pos.0).collect();
        assert_eq!(actual.as_str(), after);
        assert_eq!(actual_positions, expected_positions);
    }
    check("", "", &[]);
    check("\n", "\n", &[]);
    check("\r", "\r", &[]);
    check("\r\r", "\r\r", &[]);
    check("\r\n", "\n", &[1]);
    check("hello world", "hello world", &[]);
    check("hello\nworld", "hello\nworld", &[]);
    check("hello\r\nworld", "hello\nworld", &[6]);
    check("\r\nhello\r\nworld\r\n", "\nhello\nworld\n", &[1, 7, 13]);
    check("\r\r\n", "\r\n", &[2]);
    check("hello\rworld", "hello\rworld", &[]);
}

#[test]
fn test_trim() {
    let span = |lo: usize, hi: usize| {
        Span::new(BytePos::from_usize(lo), BytePos::from_usize(hi), SyntaxContext::root(), None)
    };

    // Various positions, named for their relation to `start` and `end`.
    let well_before = 1;
    let before = 3;
    let start = 5;
    let mid = 7;
    let end = 9;
    let after = 11;
    let well_after = 13;

    // The resulting span's context should be that of `self`, not `other`.
    let other = span(start, end).with_ctxt(SyntaxContext::from_u32(999));

    // Test cases for `trim_end`.

    assert_eq!(span(well_before, before).trim_end(other), Some(span(well_before, before)));
    assert_eq!(span(well_before, start).trim_end(other), Some(span(well_before, start)));
    assert_eq!(span(well_before, mid).trim_end(other), Some(span(well_before, start)));
    assert_eq!(span(well_before, end).trim_end(other), Some(span(well_before, start)));
    assert_eq!(span(well_before, after).trim_end(other), Some(span(well_before, start)));

    assert_eq!(span(start, mid).trim_end(other), None);
    assert_eq!(span(start, end).trim_end(other), None);
    assert_eq!(span(start, after).trim_end(other), None);

    assert_eq!(span(mid, end).trim_end(other), None);
    assert_eq!(span(mid, after).trim_end(other), None);

    assert_eq!(span(end, after).trim_end(other), None);

    assert_eq!(span(after, well_after).trim_end(other), None);

    // Test cases for `trim_start`.

    assert_eq!(span(after, well_after).trim_start(other), Some(span(after, well_after)));
    assert_eq!(span(end, well_after).trim_start(other), Some(span(end, well_after)));
    assert_eq!(span(mid, well_after).trim_start(other), Some(span(end, well_after)));
    assert_eq!(span(start, well_after).trim_start(other), Some(span(end, well_after)));
    assert_eq!(span(before, well_after).trim_start(other), Some(span(end, well_after)));

    assert_eq!(span(mid, end).trim_start(other), None);
    assert_eq!(span(start, end).trim_start(other), None);
    assert_eq!(span(before, end).trim_start(other), None);

    assert_eq!(span(start, mid).trim_start(other), None);
    assert_eq!(span(before, mid).trim_start(other), None);

    assert_eq!(span(before, start).trim_start(other), None);

    assert_eq!(span(well_before, before).trim_start(other), None);
}

#[test]
fn test_unnormalized_source_length() {
    let source = "\u{feff}hello\r\nferries\r\n".to_owned();
    let sf = SourceFile::new(
        FileName::Anon(Hash64::ZERO),
        source,
        SourceFileHashAlgorithm::Sha256,
        Some(SourceFileHashAlgorithm::Sha256),
    )
    .unwrap();
    assert_eq!(sf.unnormalized_source_len, 19);
    assert_eq!(sf.normalized_source_len.0, 14);
}
