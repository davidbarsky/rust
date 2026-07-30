//@ needs-target-std

use run_make_support::{rfs, rustc};

fn main() {
    rfs::copy("a_first.rs", "a.rs");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("-Ztime-passes")
        .env("RUSTC_LOG", "rustc_metadata::rmeta::encoder=debug")
        .incremental("incremental")
        .run()
        .assert_stderr_contains("generate_crate_metadata")
        .assert_stderr_contains("planned RDR metadata definition and hygiene closure");

    rfs::copy("a_second.rs", "a.rs");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("-Ztime-passes")
        .env("RUSTC_LOG", "rustc_metadata::rmeta::encoder=debug")
        .incremental("incremental")
        .run()
        .assert_stderr_not_contains("generate_crate_metadata")
        .assert_stderr_contains("collecting RDR span positions without emitting rmeta");

    rfs::copy("a_third.rs", "a.rs");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .arg("-Ztime-passes")
        .env("RUSTC_LOG", "rustc_metadata::rmeta::encoder=debug")
        .incremental("incremental")
        .run()
        .assert_stderr_not_contains("generate_crate_metadata")
        .assert_stderr_contains("planned RDR metadata definition and hygiene closure")
        .assert_stderr_contains("collecting RDR span positions without emitting rmeta");
}
