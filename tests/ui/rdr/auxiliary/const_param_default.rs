//@ compile-flags: -Zrdr

#![crate_type = "rlib"]

pub struct Defaulted<const N: usize = 3>(pub [u8; N]);
