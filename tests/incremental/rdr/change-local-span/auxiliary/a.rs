//@ compile-flags: -Zquery-dep-graph -Zrdr -Cdebuginfo=0
//@ no-prefer-dynamic
#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

#[cfg(bpass1)]
fn local() {
    let value = 1;
    let _ = value;
}

#[cfg(bpass2)]
fn local() {

    let value = 1;
    let _ = value;
}

pub fn value() -> u32 {
    1
}
