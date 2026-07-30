//@ aux-build: nested_async_fn_once.rs
//@ compile-flags: -Zrdr
//@ edition: 2024
//@ check-pass

extern crate nested_async_fn_once;

async fn use_nested_async_fn_once() {
    nested_async_fn_once::invoke(async |_token| {}).await;
}

fn main() {}
