extern crate a;

pub fn values() -> (u32, u32) {
    let _ = (a::Alpha, a::Bravo);
    (a::ALPHA, a::BRAVO)
}
