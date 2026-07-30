macro_rules! value {
    ($value:expr) => {
        $value
    };
}

macro_rules! public_type {
    ($name:ident) => {
        pub struct $name;
    };
}

public_type!(Alpha);
public_type!(Bravo);
pub const ALPHA: u32 = value!(11);
pub const BRAVO: u32 = value!(22);
