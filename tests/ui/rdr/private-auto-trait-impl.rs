//@ aux-build: private_auto_trait_impl.rs
//@ compile-flags: -Zrdr
//@ check-pass

extern crate private_auto_trait_impl;

fn assert_sync<T: Sync>() {}

fn main() {
    assert_sync::<private_auto_trait_impl::Public>();
}
