//@ compile-flags: -Zrdr

#![crate_type = "rlib"]

pub struct NamedFields {
    pub first: usize,
    pub second: usize,
}
