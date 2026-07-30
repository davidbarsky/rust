//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

use std::ops::Add;

#[cfg(bpass1)]
pub fn combine<T: Add<Output = T>>(left: T, right: T) -> T {
    left + right
}

#[cfg(bpass2)]
pub fn combine<T: Add<Output = T>>(left: T, right: T) -> T {
    right + left
}
