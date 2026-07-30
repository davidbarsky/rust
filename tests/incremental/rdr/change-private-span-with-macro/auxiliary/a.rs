//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "reused", rmeta(reused))]

// The two copies model the unchanged exported macro moving after a private-only edit.
#[cfg(bpass1)]
#[macro_export]
macro_rules! value {
    () => {
        42
    };
}

#[cfg(bpass1)]
fn private() {
    let value = 1;
    let _ = value;
}

#[cfg(bpass2)]
fn private() {
    let value = stringify!(1);
    let _ = value;
}

#[cfg(bpass2)]
#[macro_export]
macro_rules! value {
    () => {
        42
    };
}
