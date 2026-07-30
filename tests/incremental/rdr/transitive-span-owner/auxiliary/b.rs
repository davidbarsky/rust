//@ aux-build: a.rs
//@ proc-macro: ../../auxiliary/location.rs
//@ compile-flags: -Zrdr
//@ no-prefer-dynamic
//@ [bpass2] rustc-not-invoked
//@ [bpass2] rdr-rmeta: same
//@ [bpass2] rdr-spans: same
//@ [bpass2] rdr-source: b.rs same

#![crate_name = "b"]
#![crate_type = "rlib"]
#![feature(decl_macro)]

extern crate a;
extern crate location;

a::define_b_value!();
