//@ revisions: bpass1 bpass2
//@ should-fail: expected RDR artifact bytes to change: `{{build-base}}/rdr/transitive-span-owner/main/auxiliary/liba.source-bundle/a.rs`
//@ aux-build: b.rs
//@ proc-macro: ../../auxiliary/location.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ [bpass2] rdr-rmeta: different
//@ [bpass2] rdr-source: main.rs same

#![crate_type = "rlib"]

extern crate b;
extern crate location;

pub const VALUE: u32 = b::b_value!();
