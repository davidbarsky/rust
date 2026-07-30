//@ aux-build: async_fn_once.rs
//@ compile-flags: -Zrdr
//@ edition: 2024
//@ check-pass

extern crate async_fn_once;

async fn use_async_fn_once() {
    async_fn_once::invoke(async |value| {
        let _: u8 = value;
    })
    .await;
}

fn main() {}
