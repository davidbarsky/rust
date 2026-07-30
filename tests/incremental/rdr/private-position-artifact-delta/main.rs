//@ revisions: bpass1 bpass2
//@ should-fail: expected RDR artifact bytes to change: `{{build-base}}/rdr/private-position-artifact-delta/main/auxiliary/liba.source-bundle/a.rs`
//@ aux-build: b.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate b;

pub use b::*;
