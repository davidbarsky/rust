//@ compile-flags: --check-cfg cfg(v2)
//@ [bfail2] compile-flags: --cfg v2
//@ [bpass3] compile-flags: --cfg v2

#[cfg(v2)]
pub fn value() -> u32 {
    2
}

#[cfg(not(v2))]
pub fn value() -> u32 {
    1
}
