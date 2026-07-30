//@ revisions: bpass1 bpass2
//@ should-fail: expected RDR artifact bytes to change: `{{build-base}}/rdr/semantic-propagation-continue/main/auxiliary/libb.spans`
//@ aux-build: b.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ [bpass2] rdr-rmeta: different
//@ [bpass2] rdr-spans: same

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

extern crate b;

pub use b::observe;
