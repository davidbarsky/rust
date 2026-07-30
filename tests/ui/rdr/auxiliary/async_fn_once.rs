//@ compile-flags: -Zrdr
//@ edition: 2024

#![crate_type = "rlib"]

pub async fn invoke<F>(f: F)
where
    F: AsyncFnOnce(u8),
{
    f(0).await;
}
