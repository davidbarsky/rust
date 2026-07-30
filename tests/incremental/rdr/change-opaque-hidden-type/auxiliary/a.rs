//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ edition: 2021

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
pub fn values() -> impl Iterator<Item = u32> {
    std::iter::once(1)
}

#[cfg(bpass2)]
pub fn values() -> impl Iterator<Item = u32> {
    [1, 2].into_iter()
}
