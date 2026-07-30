//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
pub trait Value {
    fn value(&self) -> u32;
}

#[cfg(bpass2)]
pub trait Value {
    fn value(&self) -> u64;
}
