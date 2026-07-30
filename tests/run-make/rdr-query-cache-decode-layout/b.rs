#![feature(rustc_attrs)]

extern crate a;

#[rustc_clean(cfg = "second")]
pub fn use_public() {
    let _: Option<a::Public> = None;
}
