//@ aux-build: provider.rs
//@ revisions: bpass1 bpass2
//@ should-fail: rustc was invoked for `$DIR/auxiliary/provider.rs`

extern crate provider;

fn main() {
    provider::value();
}
