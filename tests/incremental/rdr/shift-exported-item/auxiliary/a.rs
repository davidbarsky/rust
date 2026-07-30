//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

macro_rules! define_value {
    () => {
        pub fn value() -> u32 {
            1
        }
    };
}

#[cfg(bpass1)]
define_value!();

// The second revision moves the same exported item past this source text.


#[cfg(bpass2)]
define_value!();
