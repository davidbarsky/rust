//@ aux-build: private_trait_impl_monomorphization.rs
//@ compile-flags: -Zrdr
//@ run-pass

extern crate private_trait_impl_monomorphization;

fn main() {
    assert_eq!(private_trait_impl_monomorphization::find(7), 7);
}
