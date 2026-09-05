//@ aux-build: consumer.rs
//@ revisions: bpass1 bpass2
// ignore-tidy-linelength
//@ should-fail: provider `$DIR/auxiliary/derive.rs` was built without measured artifacts, so a skip cannot be justified

extern crate consumer;

fn main() {
    consumer::value();
}
