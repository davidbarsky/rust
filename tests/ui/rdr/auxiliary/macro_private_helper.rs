//@ compile-flags: -Zrdr

#![feature(decl_macro)]
#![crate_type = "rlib"]

pub mod env {
    mod __macro_refs {
        pub macro helper() {
            42
        }

        pub macro exported() {
            $crate::env::__macro_refs::helper!()
        }
    }

    pub use self::__macro_refs::exported;
}
