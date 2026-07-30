//@ aux-build: a.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ [bpass2] rdr-rmeta: different
//@ [bpass2] rdr-spans: different
//@ [bpass2] rdr-source: b.rs same

#![feature(rustc_attrs)]
#![crate_name = "b"]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

extern crate a;

pub fn observe(_: a::Input) {}
