//@ revisions: bpass1 bpass2
//@ should-fail: named source `{{build-base}}/rdr/private-position-artifact-delta/main/auxiliary/liba.source-bundle/a.rs` is absent from both the previous and current snapshots
//@ aux-build: b.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate b;

pub use b::*;
