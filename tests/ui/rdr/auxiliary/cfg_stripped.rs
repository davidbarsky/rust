//@ compile-flags: -Zrdr

#![crate_type = "rlib"]

pub mod api {
    #[cfg(false)]
    pub fn unavailable() {}

    pub fn value_namespace() {}

    #[cfg(false)]
    #[allow(non_camel_case_types)]
    pub type value_namespace = ();
}
