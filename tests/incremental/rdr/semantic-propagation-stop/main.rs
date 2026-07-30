//@ revisions: bpass1 bpass2
//@ should-fail: expected RDR artifact bytes to be unchanged: `{{build-base}}/rdr/semantic-propagation-stop/main/auxiliary/libb.rmeta`
//@ aux-build: b.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate b;

pub use b::*;
