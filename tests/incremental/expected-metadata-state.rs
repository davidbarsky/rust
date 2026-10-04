//@ revisions: bpass1 bpass2 bpass3 bpass4
//@ compile-flags: -Z query-dep-graph
//@ [bpass4] compile-flags: -C overflow-checks=no

#![feature(rustc_attrs)]
#![allow(internal_features)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass1", state = "discarded")]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "reused")]
#![rustc_expected_metadata_state(cfg = "bpass3", state = "changed")]
#![rustc_expected_metadata_state(cfg = "bpass4", state = "discarded")]

#[cfg(any(bpass1, bpass2))]
pub fn value() -> u32 {
    1
}

#[cfg(any(bpass3, bpass4))]
pub fn value() -> u32 {
    2
}
