//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

#[cfg(bpass1)]
pub trait Value {
    fn value(&self) -> u32;
}

#[cfg(bpass2)]
pub trait Value {
    fn value(&self) -> u64;
}
