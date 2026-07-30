//@ ignore-cross-compile
//@ ignore-windows
//@ needs-target-std

//@ should-fail: rmake recipe failed to complete
use run_make_support::{cwd, is_darwin, llvm_dwarfdump, rfs, run, rust_lib_name, rustc};

fn main() {
    rfs::copy("a_first.rs", "a.rs");
    let mut first = rustc();
    first
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("first")
        .arg("-Zrdr")
        .debuginfo("2")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta");
    if is_darwin() {
        first.split_debuginfo("off");
    }
    first.run();
    let first_rmeta = rfs::read("liba.rmeta");

    let mut first_middle = rustc();
    first_middle
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", rust_lib_name("a"))
        .arg("-Zrdr")
        .debuginfo("2")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("b-incremental");
    if is_darwin() {
        first_middle.split_debuginfo("off");
    }
    first_middle.run();

    rfs::copy("a_second.rs", "a.rs");
    let mut second = rustc();
    second
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("second")
        .arg("-Zrdr")
        .debuginfo("2")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta");
    if is_darwin() {
        second.split_debuginfo("off");
    }
    second.run();
    assert_eq!(rfs::read("liba.rmeta"), first_rmeta);

    let mut second_middle = rustc();
    second_middle
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", rust_lib_name("a"))
        .arg("-Zrdr")
        .debuginfo("2")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("b-incremental");
    if is_darwin() {
        second_middle.split_debuginfo("off");
    }
    second_middle.run();

    let mut downstream = rustc();
    downstream
        .input("main.rs")
        .extern_("b", rust_lib_name("b"))
        .arg("-Zrdr")
        .debuginfo("2")
        .remap_path_prefix(cwd(), "/rdr");
    if is_darwin() {
        downstream.split_debuginfo("off");
    }
    downstream.run();

    run("main");

    llvm_dwarfdump()
        .input(rust_lib_name("a"))
        .run()
        .assert_stdout_contains("/rdr/a.rs")
        .assert_stdout_not_contains(cwd().display().to_string());
    llvm_dwarfdump()
        .input(rust_lib_name("b"))
        .run()
        .assert_stdout_contains("/rdr/a.rs")
        .assert_stdout_contains("DW_AT_decl_line\t(41)")
        .assert_stdout_contains("DW_AT_name\t(\"b.rs/")
        .assert_stdout_contains("DW_AT_comp_dir\t(\"/rdr\")")
        .assert_stdout_not_contains(cwd().display().to_string());
}
