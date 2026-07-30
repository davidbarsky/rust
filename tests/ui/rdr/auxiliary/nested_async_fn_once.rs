//@ compile-flags: -Zrdr
//@ edition: 2024

#![crate_type = "rlib"]

pub struct Token<'a>(&'a ());

async fn with_token<F, R>(f: F) -> R
where
    F: for<'a> AsyncFnOnce(Token<'a>) -> R,
{
    let value = ();
    f(Token(&value)).await
}

pub async fn invoke<F, R>(f: F) -> R
where
    F: for<'a> AsyncFnOnce(Token<'a>) -> R,
{
    with_token(async |token| f(token).await).await
}
