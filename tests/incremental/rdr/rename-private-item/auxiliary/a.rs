//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

pub fn value() -> u32 {
    1
}

#[cfg(bpass1)]
fn before() {}

#[cfg(bpass2)]
fn after() {}
