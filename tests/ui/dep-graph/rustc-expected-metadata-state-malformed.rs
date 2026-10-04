//@ compile-flags: -Z query-dep-graph

#![feature(rustc_attrs)]
#![rustc_expected_metadata_state] //~ ERROR malformed
#![rustc_expected_metadata_state(cfg = "a")] //~ ERROR missing a `state` argument
#![rustc_expected_metadata_state(state = "reused")] //~ ERROR missing a `cfg` argument
#![rustc_expected_metadata_state(cfg = "a", state = "rebuilt")]
//~^ ERROR malformed
//~| ERROR missing a `state` argument
#![rustc_expected_metadata_state(cfg = "a", state = "reused", state = "changed")]
//~^ ERROR malformed
#![rustc_expected_metadata_state(cfg = "a", cfg = "b", state = "reused")]
//~^ ERROR malformed
#![rustc_expected_metadata_state(cfg = "a", metadata_hash = "reused")]
//~^ ERROR malformed
//~| ERROR missing a `state` argument
#![rustc_expected_metadata_state(cfg = "a", state = reused)]
//~^ ERROR expected a literal

#[rustc_expected_metadata_state(cfg = "a", state = "reused")]
//~^ ERROR crate-level attribute should be an inner attribute
fn main() {}
