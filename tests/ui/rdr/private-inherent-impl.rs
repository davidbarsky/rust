//@ compile-flags: -Zrdr
//@ check-pass

#![crate_type = "rlib"]
#![allow(dead_code)]

struct Private;

impl Private {
    fn method() {}
}

pub fn exported() {}
