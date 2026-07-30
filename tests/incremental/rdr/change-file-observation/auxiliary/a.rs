//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

mod auxiliary {
    #[cfg(bpass1)]
    pub mod file_observation_first;
    #[cfg(bpass2)]
    pub mod file_observation_second;
}

#[cfg(bpass1)]
pub const LOCATION: &str = auxiliary::file_observation_first::LOCATION;
#[cfg(bpass2)]
pub const LOCATION: &str = auxiliary::file_observation_second::LOCATION;
