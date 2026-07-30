//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "reused")]

#[cfg(bpass1)]
fn unused() {}

pub fn value() -> u32 {
    1
}
