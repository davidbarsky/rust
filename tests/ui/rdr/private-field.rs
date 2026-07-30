//@ compile-flags: -Zrdr
//@ check-pass

#![crate_type = "rlib"]
#![allow(dead_code)]

pub struct Public {
    private: u32,
}
