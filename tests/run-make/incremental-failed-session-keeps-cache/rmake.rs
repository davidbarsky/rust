use run_make_support::rustc;

fn main() {
    rustc().input("lib.rs").crate_type("rlib").incremental("incr").run();
    rustc()
        .input("lib.rs")
        .crate_type("rlib")
        .incremental("incr")
        .cfg("broken")
        .arg("-Coverflow-checks=no")
        .run_fail();
    rustc()
        .input("lib.rs")
        .crate_type("rlib")
        .incremental("incr")
        .arg("-Zassert-incr-state=loaded")
        .run();
}
