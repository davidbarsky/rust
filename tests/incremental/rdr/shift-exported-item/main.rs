//@ revisions: bpass1 bpass2
//@ aux-build: a.rs
//@ compile-flags: -Zrdr
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate a;

pub use a::*;
