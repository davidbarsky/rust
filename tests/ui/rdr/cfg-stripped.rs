//@ aux-build: cfg_stripped.rs
//@ compile-flags: -Zrdr

extern crate cfg_stripped;

fn main() {
    cfg_stripped::api::unavailable();
    //~^ ERROR cannot find function `unavailable` in module `cfg_stripped::api`
    //~| NOTE found an item that was configured out
    //~| NOTE not found in `cfg_stripped::api`

    let _: cfg_stripped::api::value_namespace;
    //~^ ERROR cannot find type `value_namespace` in module `cfg_stripped::api`
    //~| NOTE a function named `cfg_stripped::api::value_namespace` exists in another namespace
    //~| NOTE found an item that was configured out
    //~| NOTE not found in `cfg_stripped::api`
}
