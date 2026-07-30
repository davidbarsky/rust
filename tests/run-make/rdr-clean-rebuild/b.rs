extern crate a;

#[inline(never)]
pub fn value() -> u32 {
    a::value()
}
