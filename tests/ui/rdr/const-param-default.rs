//@ aux-build: const_param_default.rs
//@ compile-flags: -Zrdr
//@ check-pass

extern crate const_param_default;

fn main() {
    let _: const_param_default::Defaulted = const_param_default::Defaulted([0; 3]);
}
