//@ compile-flags: --check-cfg cfg(changed)
//@ [bpass2] compile-flags: --cfg changed
//@ [bpass2] rustc-not-invoked

#[cfg(changed)]
pub fn value() -> u32 {
    2
}

#[cfg(not(changed))]
pub fn value() -> u32 {
    1
}
