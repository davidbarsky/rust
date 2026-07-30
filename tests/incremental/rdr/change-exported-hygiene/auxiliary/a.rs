//@ compile-flags: -Zquery-dep-graph -Zrdr
//@ no-prefer-dynamic
//@ proc-macro: ../../auxiliary/location.rs

#![feature(rustc_attrs)]
#![crate_type = "rlib"]
#![rustc_expected_metadata_state(cfg = "bpass2", metadata_hash = "changed", rmeta(rebuilt))]

#[cfg(bpass1)]
location::define_hygiene!(call);

#[cfg(bpass2)]
location::define_hygiene!(mixed);
