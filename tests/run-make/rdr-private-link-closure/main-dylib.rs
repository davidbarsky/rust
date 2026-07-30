extern crate a;
extern crate b;

fn main() {
    assert_eq!(b::value(), 1);
    assert_eq!(a::next_value(), 2);
}
