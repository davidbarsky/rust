//@ aux-build: private_macro_helper_reexport.rs
//@ compile-flags: -Zrdr
//@ run-pass

extern crate private_macro_helper_reexport;

fn main() {
    let _: String = private_macro_helper_reexport::env::make_string!();
    assert!(private_macro_helper_reexport::env::compare!(1, 1));
}
