//@ revisions: bpass1 bpass2
//@ should-fail: failed to read RDR spans artifact `{{build-base}}/rdr/private-position-artifact-delta/main/auxiliary/liba.spans`: No such file or directory (os error 2)
//@ aux-build: b.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate b;

pub use b::*;
