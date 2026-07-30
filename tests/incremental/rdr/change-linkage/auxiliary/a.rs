//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(linkage, rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

#[cfg(bpass1)]
pub static VALUE: u32 = 1;

#[cfg(bpass2)]
#[linkage = "weak_odr"]
pub static VALUE: u32 = 1;
