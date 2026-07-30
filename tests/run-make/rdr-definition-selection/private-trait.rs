#![allow(ambiguous_glob_imports, dead_code, unused_imports)]

pub struct Public;

pub fn exported() -> u32 {
    0
}

mod left {
    pub fn duplicate() {}
}

mod right {
    pub fn duplicate() {}
}

trait PrivateTrait {
    fn private_method(&self);
}
