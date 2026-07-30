macro_rules! value {
    ($value:expr) => {
        $value
    };
}

macro_rules! different_public_type {
    ($name:ident) => {
        pub struct $name;
    };
}

different_public_type!(Alpha);
different_public_type!(Bravo);
pub const ALPHA: u32 = value!(11);
pub const BRAVO: u32 = value!(22);
