#![allow(dead_code)]

fn private_value() -> u32 {
    2
}

fn alpha() {}

fn beta() {}

#[inline(never)]
pub fn value() -> u32 {
    private_value()
}
