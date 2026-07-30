//@ revisions: bpass1 bpass2
//@ should-fail: auxiliary build of {{cwd}}/tests/incremental/rdr/transitive-span-owner/auxiliary/a_second.rs failed to compile:
//@ aux-build: b.rs
//@ proc-macro: ../../auxiliary/location.rs
//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ [bpass2] rdr-rmeta: different
//@ [bpass2] rdr-source: main.rs same

#![crate_type = "rlib"]

extern crate b;
extern crate location;

pub const VALUE: u32 = b::b_value!();
