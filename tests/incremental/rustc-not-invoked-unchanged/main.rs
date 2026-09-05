//@ aux-build: provider.rs
//@ revisions: bpass1 bpass2

extern crate provider;

fn main() {
    provider::value();
}
