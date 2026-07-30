//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

#[cfg(bpass1)]
#[link(name = "rdr_native_one")]
unsafe extern "C" {
    pub fn rdr_native();
}

#[cfg(bpass2)]
#[link(name = "rdr_native_two")]
unsafe extern "C" {
    pub fn rdr_native();
}
