//@ should-fail: rmake recipe failed to complete
use run_make_support::{rfs, rust_lib_name, rustc};

fn main() {
    rfs::copy("a_first.rs", "a.rs");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("first")
        .arg("-Zrdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta")
        .run();
    let first_rmeta = rfs::read("liba.rmeta");

    rfs::copy("a_second.rs", "a.rs");
    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", rust_lib_name("a"))
        .arg("-Zrdr")
        .incremental("b-incremental")
        .run();

    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("second")
        .arg("-Zrdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta")
        .run();
    assert_eq!(rfs::read("liba.rmeta"), first_rmeta);

    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", rust_lib_name("a"))
        .cfg("second")
        .arg("-Zrdr")
        .arg("--error-format=json")
        .incremental("b-incremental")
        .run_fail()
        .assert_stderr_contains(r#""file_name":"a.rs","byte_start":"#)
        .assert_stderr_contains(r#""line_start":9"#);
}
