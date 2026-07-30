#![allow(dead_code)]

fn beta() {}

fn private_value() -> u32 {
    2
}

fn alpha() {}

#[inline(never)]
pub fn value() -> u32 {
    private_value()
}
