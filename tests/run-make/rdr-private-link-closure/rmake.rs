//@ ignore-cross-compile
//@ needs-dynamic-linking
//@ needs-target-std

use run_make_support::{dynamic_lib_name, rfs, run, rust_lib_name, rustc};

fn main() {
    rustc().input("a.rs").crate_name("a").crate_type("rlib").arg("-Zrdr").run();

    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", rust_lib_name("a"))
        .run();

    rustc()
        .input("main.rs")
        .arg("-Zrdr")
        .extern_("b", rust_lib_name("b"))
        .library_search_path(".")
        .run();

    run("main");

    rfs::remove_file(rust_lib_name("b"));
    let dylib = dynamic_lib_name("b-dylib");
    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("dylib")
        .arg("-Zrdr")
        .arg("-Cprefer-dynamic")
        .extern_("a", rust_lib_name("a"))
        .output(&dylib)
        .run();

    // B's semantic metadata deliberately omits A. C names A directly here so the dylib link
    // closure must record that B already owns A's static copy.
    let link_args = rustc()
        .input("main-dylib.rs")
        .arg("-Zrdr")
        .extern_("a", rust_lib_name("a"))
        .extern_("b", &dylib)
        .library_search_path(".")
        .print("link-args")
        .run_unchecked();
    link_args.assert_stdout_not_contains(rust_lib_name("a"));

    rustc()
        .input("main-dylib.rs")
        .arg("-Zrdr")
        .extern_("a", rust_lib_name("a"))
        .extern_("b", &dylib)
        .library_search_path(".")
        .output("main-dylib")
        .run();

    run("main-dylib");
}
