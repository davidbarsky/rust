//@ aux-build: a.rs
//@ compile-flags: -Zrdr
//@ no-prefer-dynamic
//@ [bpass2] rustc-not-invoked

#![crate_name = "b"]
#![crate_type = "rlib"]

extern crate a;

pub fn value() -> u32 {
    a::value()
}
