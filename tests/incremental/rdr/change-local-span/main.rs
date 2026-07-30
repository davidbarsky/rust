//@ revisions: bpass1 bpass2
//@ should-fail: failed to read RDR spans artifact `{{build-base}}/rdr/change-local-span/main/auxiliary/liba.spans`: No such file or directory (os error 2)
//@ aux-build: a.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate a;

pub use a::*;
