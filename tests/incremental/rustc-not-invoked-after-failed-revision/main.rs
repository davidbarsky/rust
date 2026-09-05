//@ revisions: bpass1 bfail2 bpass3
//@ aux-build: provider.rs
//@ compile-flags: --check-cfg cfg(broken)
//@ [bfail2] compile-flags: --cfg broken
//@ [bpass3] rustc-not-invoked
//@ should-fail: rustc was invoked for `$DIR/main.rs`

extern crate provider;

#[cfg(broken)]
const BROKEN: u32 = "";
//[bfail2]~^ ERROR mismatched types

fn main() {
    provider::value();
}
