//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ [bpass1] revision-source: a_first.rs
//@ [bpass2] revision-source: a_second.rs
//@ [bpass2] expect-rmeta: same
//@ [bpass2] expect-spans: different
//@ [bpass2] expect-source: a.rs different
