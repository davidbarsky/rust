//@ compile-flags: -Zrdr
//@ check-pass

#![crate_type = "rlib"]

mod private {
    mod inner {
        pub(crate) struct Private;
    }

    pub(crate) use self::inner::Private;
}

pub mod public {
    use crate::private::Private;

    pub struct Public {
        private: Private,
    }
}
