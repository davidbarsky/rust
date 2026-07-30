//@ needs-target-std

use run_make_support::{rfs, rustc};

fn main() {
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("--emit=metadata=default.rmeta")
        .run();
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .target_cpu("generic")
        .arg("--emit=metadata=generic.rmeta")
        .run();

    assert_ne!(rfs::read("default.rmeta"), rfs::read("generic.rmeta"));
}
