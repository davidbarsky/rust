//@ revisions: bpass1 bpass2 bpass3
//@ aux-build: outer.rs
//@ [bpass3] rustc-not-invoked

extern crate outer;

fn main() {
    outer::value();
}
