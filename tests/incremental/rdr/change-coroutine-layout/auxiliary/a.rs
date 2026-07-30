//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ edition: 2021

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

pub async fn value() -> u32 {
    #[cfg(bpass1)]
    let value = 1_u32;
    #[cfg(bpass2)]
    let value = 1_u64;

    async {}.await;
    value as u32
}
