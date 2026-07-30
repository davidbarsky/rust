//@ aux-build: named_fields.rs
//@ compile-flags: -Zrdr
//@ check-pass

extern crate named_fields;

fn main() {
    let _ = named_fields::NamedFields { first: 0, second: 1 };
}
