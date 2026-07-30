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

public_type!(Bravo);
public_type!(Alpha);
pub const BRAVO: u32 = value!(22);
pub const ALPHA: u32 = value!(11);
