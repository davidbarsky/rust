//@ compile-flags: -Zrdr
//@ check-pass
//@ edition: 2018

#![crate_type = "rlib"]

pub mod nested {
    pub use bool;
    pub use char as my_char;
}

pub use i32 as my_i32;
pub use str;
