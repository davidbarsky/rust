//@ revisions: bpass1 bpass2
//@ [bpass2] rustc-not-invoked

#![crate_type = "rlib"]

pub fn value() -> u32 {
    1
}
