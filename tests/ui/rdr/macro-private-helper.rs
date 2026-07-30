//@ aux-build: macro_private_helper.rs
//@ compile-flags: -Zrdr
//@ check-pass

extern crate macro_private_helper;

fn main() {
    let value: u32 = macro_private_helper::env::exported!();
    assert_eq!(value, 42);
}
