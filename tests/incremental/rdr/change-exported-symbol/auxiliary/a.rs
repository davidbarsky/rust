//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

#[cfg(bpass1)]
#[unsafe(export_name = "rdr_value_one")]
pub extern "C" fn value() -> u32 {
    1
}

#[cfg(bpass2)]
#[unsafe(export_name = "rdr_value_two")]
pub extern "C" fn value() -> u32 {
    1
}
