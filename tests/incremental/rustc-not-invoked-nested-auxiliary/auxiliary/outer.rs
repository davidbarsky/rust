//@ aux-build: inner.rs
//@ [bpass3] rustc-not-invoked

extern crate inner;

pub fn value() -> u32 {
    inner::value()
}
