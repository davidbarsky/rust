//@ ignore-cross-compile
//@ needs-target-std

//@ should-fail: rmake recipe failed to complete
use run_make_support::{rfs, rustc};

fn main() {
    for (source, output) in
        [("first.rs", "first"), ("reordered.rs", "reordered"), ("changed.rs", "changed")]
    {
        rfs::copy(source, "a.rs");
        rfs::create_dir(output);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .arg("-Zrdr")
            .emit("metadata")
            .out_dir(output)
            .run();
    }

    assert!(
        rfs::read("first/liba.spans") != rfs::read("reordered/liba.spans"),
        "reordering the selected expansions did not change their source positions"
    );

    rustc()
        .input("consumer.rs")
        .crate_name("consumer")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", "reordered/liba.rmeta")
        .emit("metadata")
        .run();

    assert!(
        rfs::read("first/liba.rmeta") == rfs::read("reordered/liba.rmeta"),
        "reordering selected expansions changed their content-derived metadata identity"
    );
    assert!(
        rfs::read("first/liba.rmeta") != rfs::read("changed/liba.rmeta"),
        "changing expansion semantics did not change content-derived metadata identity"
    );

    rfs::copy("first.rs", "a.rs");
    rfs::create_dir("incremental-first");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("-Ztime-passes")
        .env("RUSTC_LOG", "rustc_metadata::rmeta::encoder=debug")
        .emit("metadata")
        .out_dir("incremental-first")
        .incremental("incremental-cache")
        .run()
        .assert_stderr_contains("generate_crate_metadata");

    rfs::copy("reordered.rs", "a.rs");
    rfs::create_dir("incremental-reordered");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("-Ztime-passes")
        .env("RUSTC_LOG", "rustc_metadata::rmeta::encoder=debug")
        .emit("metadata")
        .out_dir("incremental-reordered")
        .incremental("incremental-cache")
        .run()
        .assert_stderr_not_contains("generate_crate_metadata")
        .assert_stderr_contains("collecting RDR span positions without emitting rmeta");

    assert_eq!(
        rfs::read("incremental-reordered/liba.rmeta"),
        rfs::read("reordered/liba.rmeta"),
        "incremental reuse changed the content-derived metadata encoding"
    );
    assert_ne!(
        rfs::read("incremental-first/liba.spans"),
        rfs::read("incremental-reordered/liba.spans"),
        "incremental reuse did not refresh expansion positions"
    );
}
