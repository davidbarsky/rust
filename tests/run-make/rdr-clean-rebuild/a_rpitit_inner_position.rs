#![feature(custom_inner_attributes)]
#![rustfmt::skip]

pub trait Factory {
    fn alpha() -> /* source position only */ impl Copy;
    fn beta() -> /* source position only */ impl Copy;
    fn gamma() -> /* source position only */ impl Copy;
}

pub struct Public;

impl Factory for Public {
    fn alpha() -> /* source position only */ impl Copy {
        1u8
    }

    fn beta() -> /* source position only */ impl Copy {
        2u16
    }

    fn gamma() -> /* source position only */ impl Copy {
        3u32
    }
}
