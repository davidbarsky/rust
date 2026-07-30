//@ revisions: bpass1 bpass2
//@ aux-build: b.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate b;

pub use b::*;
