//@ needs-target-std

use run_make_support::{rfs, rustc};

fn main() {
    rfs::create_dir("first");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extra_filename("-first")
        .emit("metadata")
        .output("first/liba.rmeta")
        .run();

    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", "first/liba.rmeta")
        .arg("-Zquery-dep-graph")
        .incremental("b-incremental")
        .run();

    rfs::create_dir("second");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extra_filename("-second")
        .emit("metadata")
        .output("second/liba.rmeta")
        .run();

    assert_ne!(rfs::read("first/liba.rmeta"), rfs::read("second/liba.rmeta"));

    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", "second/liba.rmeta")
        .cfg("second")
        .env("RUST_DEP_GRAPH", "b-dep-graph.gv")
        .arg("-Zquery-dep-graph")
        .arg("-Zdump-dep-graph")
        .arg("-Zincremental-info")
        .incremental("b-incremental")
        .run()
        .assert_stderr_contains_regex(
            r"metadata_decode_layout_id\s+\|[^|\n]+\|[^|\n]+\|\s+0\.0 \|",
        );

    assert!(
        rfs::read_to_string("b-dep-graph.gv.txt")
            .lines()
            .any(|edge| edge == "type_of -> metadata_decode_layout_id")
    );
}
