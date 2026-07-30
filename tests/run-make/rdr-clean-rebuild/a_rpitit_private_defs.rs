#![feature(custom_inner_attributes)]
#![rustfmt::skip]
#![allow(dead_code)]

fn private_alpha() {}
fn private_beta() {}
fn private_gamma() {}
fn private_delta() {}

pub trait Factory {
    fn alpha() -> impl Copy;
    fn beta() -> impl Copy;
    fn gamma() -> impl Copy;
}

pub struct Public;

impl Factory for Public {
    fn alpha() -> impl Copy {
        1u8
    }

    fn beta() -> impl Copy {
        2u16
    }

    fn gamma() -> impl Copy {
        3u32
    }
}
