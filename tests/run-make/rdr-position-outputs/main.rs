extern crate b;

fn main() {
    let expected_line = line!() + 1;
    let location = b::tracked();
    assert_eq!(location.line(), expected_line);
    assert!(location.file().ends_with("main.rs"));
    assert_eq!(b::generic(1), 1);
    assert_eq!(b::imported(2), 2);
}
