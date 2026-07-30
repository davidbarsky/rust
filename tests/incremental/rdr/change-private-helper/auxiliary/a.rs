//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

#[cfg(bpass1)]
fn helper() -> u32 {
    1
}

#[cfg(bpass2)]
fn helper() -> u32 {
    2
}

#[inline(never)]
pub fn value() -> u32 {
    helper()
}
