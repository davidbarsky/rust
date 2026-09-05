extern crate proc_macro;

use proc_macro::TokenStream;

#[proc_macro]
pub fn value(_: TokenStream) -> TokenStream {
    #[cfg(bpass1)]
    let value = "1";
    #[cfg(bpass2)]
    let value = "2";
    value.parse().unwrap()
}
