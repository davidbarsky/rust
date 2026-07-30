//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "reused")]

#[inline(never)]
pub fn value() -> u32 {
    if cfg!(bpass1) { 1 } else { 2 }
}
