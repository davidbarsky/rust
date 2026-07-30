//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ proc-macro: ../../auxiliary/location.rs

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", state = "reused")]

macro_rules! define_value {
    () => {
        pub const VALUE: u32 = location::observe_spans!(123);
    };
}

#[cfg(bpass1)]
define_value!();


#[cfg(bpass2)]
define_value!();
