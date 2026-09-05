//@ revisions: bpass1 bpass2
//@ compile-flags: --check-cfg cfg(changed)
//@ [bpass2] compile-flags: --cfg changed
//@ [bpass2] rustc-not-invoked
//@ should-fail: rustc was invoked for `$DIR/rustc-not-invoked-main-compile-flags.rs`

#![crate_type = "rlib"]

#[cfg(changed)]
pub fn value() -> u32 {
    2
}

#[cfg(not(changed))]
pub fn value() -> u32 {
    1
}
