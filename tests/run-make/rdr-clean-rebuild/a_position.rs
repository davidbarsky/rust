#![allow(dead_code)]

fn beta() {}

fn private_value() -> u32 {
    2
}

fn alpha() {}

// Keep this source-position-only edit visible to rustfmt.
//
//
#[inline(never)]
pub fn value() -> u32 {
    private_value()
}
