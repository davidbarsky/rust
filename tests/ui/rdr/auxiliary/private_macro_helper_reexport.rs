//@ compile-flags: -Zrdr

#![crate_type = "rlib"]
#![feature(decl_macro)]

pub mod env {
    use std::cmp;

    mod __macro_refs {
        pub use std::string::String;

        pub macro make_string() {
            $crate::env::__macro_refs::String::new()
        }
    }

    pub use self::__macro_refs::make_string;

    pub macro compare($lhs:expr, $rhs:expr) {
        cmp::PartialEq::eq(&$lhs, &$rhs)
    }
}
