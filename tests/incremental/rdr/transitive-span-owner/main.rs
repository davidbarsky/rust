//@ revisions: bpass1 bpass2
//@ should-fail: named source `{{build-base}}/rdr/transitive-span-owner/main/auxiliary/liba.source-bundle/a.rs` is absent from both the previous and current snapshots
//@ aux-build: b.rs
//@ proc-macro: ../../auxiliary/location.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr

#![crate_type = "rlib"]

extern crate b;
extern crate location;

pub const VALUE: u32 = b::b_value!();
