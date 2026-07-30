//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ [bpass1] revision-source: a_first.rs
//@ [bpass2] revision-source: a_second.rs
//@ [bpass2] rdr-rmeta: same
//@ [bpass2] rdr-spans: different
//@ [bpass2] rdr-source: a.rs different
