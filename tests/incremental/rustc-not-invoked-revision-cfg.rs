//@ revisions: bpass1 bpass2
//@ [bpass2] rustc-not-invoked
//@ should-fail: test compilation failed although it shouldn't!

#![crate_type = "rlib"]

#[cfg(bpass2)]
pub fn value() -> u32 {
    2
}

#[cfg(not(bpass2))]
pub fn value() -> u32 {
    1
}
