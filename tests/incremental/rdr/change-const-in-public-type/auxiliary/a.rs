//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
const LENGTH: usize = 1;
#[cfg(bpass2)]
const LENGTH: usize = 2;

pub struct Value(pub [u8; LENGTH]);
