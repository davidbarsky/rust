//@ revisions: bpass1 bpass2
//@ aux-build: provider.rs

extern crate provider;

fn main() {
    provider::value();
}
