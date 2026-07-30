#![allow(private_interfaces)]

pub type Public = private::First;

mod private {
    pub type First = Second;
    pub type Second = Third;
    pub type Third = u32;
}
