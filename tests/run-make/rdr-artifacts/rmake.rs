//@ ignore-cross-compile
//@ needs-dynamic-linking
//@ needs-target-std
//@ should-fail: rmake recipe failed to complete

use run_make_support::{dynamic_lib_name, is_darwin, path, rust_lib_name, rustc};

fn main() {
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("metadata")
        .output("liba.rmeta")
        .run();
    assert!(path("liba.spans").exists());
    rustc()
        .input("b.rs")
        .crate_name("b_from_rmeta")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", "liba.rmeta")
        .emit("metadata")
        .run();

    let rlib = rust_lib_name("a_rlib");
    rustc().input("a.rs").crate_name("a_rlib").crate_type("rlib").arg("-Zrdr").run();
    assert!(path(&rlib).with_extension("spans").exists());
    rustc()
        .input("b.rs")
        .crate_name("b_from_rlib")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", &rlib)
        .emit("metadata")
        .run();

    let dylib = dynamic_lib_name("a_dylib");
    rustc().input("a.rs").crate_name("a_dylib").crate_type("dylib").arg("-Zrdr").run();
    assert!(path(&dylib).with_extension("spans").exists());
    rustc()
        .input("b.rs")
        .crate_name("b_from_dylib")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", &dylib)
        .emit("metadata")
        .run();

    if is_darwin() {
        let dylib = dynamic_lib_name("a_dylib_with_packed_debuginfo");
        rustc()
            .input("a.rs")
            .crate_name("a_dylib_with_packed_debuginfo")
            .crate_type("dylib")
            .arg("-Zrdr")
            .arg("-g")
            .arg("-Csplit-debuginfo=packed")
            .run();
        assert!(path(format!("{dylib}.dSYM")).exists());
    }
}
