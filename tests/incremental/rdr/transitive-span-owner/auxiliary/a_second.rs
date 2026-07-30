//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_name = "a"]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

fn private_marker() {
    let marker = 123_456;
    let _ = marker;
}

#[macro_export]
macro_rules! define_b_value {
    () => {
        pub fn generated(value: u32) -> u32 {
            if true { value } else { value }
        }

        pub macro b_value() {
            location::location!(123)
        }
    };
}
