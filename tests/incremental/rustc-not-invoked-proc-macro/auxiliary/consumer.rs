//@ proc-macro: derive.rs
//@ [bpass2] rustc-not-invoked

extern crate derive;

pub fn value() -> u32 {
    derive::value!()
}
