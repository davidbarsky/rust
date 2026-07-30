//@ aux-build: a.rs
//@ compile-flags: -Zrdr
//@ no-prefer-dynamic
//@ [bpass2] expect-rmeta: same
//@ [bpass2] expect-spans: same
//@ [bpass2] expect-source: b.rs same

#![crate_name = "b"]
#![crate_type = "rlib"]

extern crate a;

fn observe(_: a::Input) {}

pub fn value() -> u32 {
    7
}
