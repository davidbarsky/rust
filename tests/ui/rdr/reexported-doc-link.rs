//@ compile-flags: -Zrdr
//@ check-pass

#![crate_type = "rlib"]

mod inner {
    pub struct Public;

    impl Public {
        /// Returns a [`Public`].
        pub fn method() -> Public {
            Public
        }
    }
}

pub use inner::Public;
