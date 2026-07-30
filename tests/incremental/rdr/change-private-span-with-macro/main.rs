//@ revisions: bpass1 bpass2
//@ should-fail: failed to read RDR spans artifact `{{build-base}}/rdr/change-private-span-with-macro/main/auxiliary/liba.spans`: No such file or directory (os error 2)
//@ aux-build: a.rs
//@ compile-flags: -Zrdr -Cdebuginfo=2
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

extern crate a;

// Code generation observes the definition-site positions of the expanded tokens.
pub fn value() -> u32 {
    a::value!()
}
