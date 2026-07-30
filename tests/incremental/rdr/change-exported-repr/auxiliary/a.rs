//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
#[repr(C)]
pub struct Value(pub u32);

#[cfg(bpass2)]
#[repr(transparent)]
pub struct Value(pub u32);
