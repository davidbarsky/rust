//@ compile-flags: -Zrdr

#![crate_type = "rlib"]

use std::cell::UnsafeCell;

struct Inner(UnsafeCell<()>);

unsafe impl Sync for Inner {}

pub struct Public(Inner);
