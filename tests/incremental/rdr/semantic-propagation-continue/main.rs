//@ revisions: bpass1 bpass2
//@ should-fail: expected artifact bytes to change: `{{build-base}}/rdr/semantic-propagation-continue/main/auxiliary/libb.spans`
//@ aux-build: b.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

extern crate b;

pub use b::observe;
