//@ aux-build: a.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ [bpass2] expect-rmeta: different
//@ [bpass2] expect-spans: different
//@ [bpass2] expect-source: b.rs same

#![feature(rustc_attrs)]
#![crate_name = "b"]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

extern crate a;

pub fn observe(_: a::Input) {}
