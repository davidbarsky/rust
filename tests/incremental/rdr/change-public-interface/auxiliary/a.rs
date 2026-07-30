//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
pub fn value() -> u32 {
    1
}

#[cfg(bpass2)]
pub fn value() -> u64 {
    1
}
