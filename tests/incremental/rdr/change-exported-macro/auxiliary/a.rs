//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "changed")]

#[cfg(bpass1)]
#[macro_export]
macro_rules! value {
    () => {
        1
    };
}

#[cfg(bpass2)]
#[macro_export]
macro_rules! value {
    () => {
        2
    };
}
