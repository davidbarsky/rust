//@ ignore-cross-compile
//@ ignore-windows
//@ needs-profiler-runtime
//@ needs-target-std

use run_make_support::{bin_name, cmd, cwd, llvm, llvm_profdata, path, rfs, rust_lib_name, rustc};

fn main() {
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("first")
        .arg("-Zrdr")
        .arg("-Cinstrument-coverage")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta")
        .run();
    let first_rmeta = rfs::read("liba.rmeta");

    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .cfg("second")
        .arg("-Zrdr")
        .arg("-Cinstrument-coverage")
        .remap_path_prefix(cwd(), "/rdr")
        .incremental("a-incremental")
        .arg("--emit=link,metadata=liba.rmeta")
        .run();
    assert_eq!(rfs::read("liba.rmeta"), first_rmeta);

    rustc()
        .input("main.rs")
        .extern_("a", rust_lib_name("a"))
        .arg("-Zrdr")
        .arg("-Cinstrument-coverage")
        .remap_path_prefix(cwd(), "/rdr")
        .run();
    cmd(path(bin_name("main"))).env("LLVM_PROFILE_FILE", "main.profraw").run();
    llvm_profdata().merge().output("main.profdata").input("main.profraw").run();

    cmd(llvm::llvm_bin_dir().join("llvm-cov"))
        .arg("show")
        .arg(bin_name("main"))
        .arg("--instr-profile=main.profdata")
        .arg(format!("-path-equivalence=/rdr,{}", cwd().display()))
        .run()
        .assert_stdout_contains("a.rs")
        .assert_stdout_contains("pub fn generic");
}
