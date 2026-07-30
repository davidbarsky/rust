//@ ignore-cross-compile
//@ needs-target-std

use run_make_support::{rfs, rustc};

fn main() {
    for source in [
        "baseline.rs",
        "private-const.rs",
        "private-trait.rs",
        "private-trait-impl.rs",
        "private-external-trait-impl.rs",
        "private-async.rs",
        "private-inherent-method.rs",
        "private-ambiguous-globs.rs",
        "private-foreign-item.rs",
        "public-inherent-method.rs",
        "public-trait-impl.rs",
        "private-field-layout.rs",
        "private-linked-symbol.rs",
        "transitive-signature.rs",
        "transitive-signature-changed.rs",
    ] {
        let output = source.strip_suffix(".rs").unwrap();
        rfs::create_dir(output);
        rustc()
            .input(source)
            .crate_name("selection")
            .crate_type("rlib")
            .edition("2024")
            .arg("-Zrdr")
            .emit("metadata")
            .out_dir(output)
            .run();
    }

    let baseline = rfs::read("baseline/libselection.rmeta");
    for output in [
        "private-const",
        "private-trait",
        "private-trait-impl",
        "private-external-trait-impl",
        "private-async",
        "private-inherent-method",
        "private-ambiguous-globs",
        "private-foreign-item",
    ] {
        let metadata = rfs::read(format!("{output}/libselection.rmeta"));
        assert!(
            metadata.as_slice() == baseline.as_slice(),
            "{output} changed the RDR semantic metadata"
        );
    }

    for output in [
        "public-inherent-method",
        "public-trait-impl",
        "private-field-layout",
        "private-linked-symbol",
    ] {
        let metadata = rfs::read(format!("{output}/libselection.rmeta"));
        assert!(
            metadata.as_slice() != baseline.as_slice(),
            "{output} did not change the RDR semantic metadata"
        );
    }

    assert_ne!(
        rfs::read("transitive-signature/libselection.rmeta"),
        rfs::read("transitive-signature-changed/libselection.rmeta"),
        "a semantic change behind three private aliases did not change RDR metadata"
    );
    rustc()
        .input("transitive-consumer.rs")
        .edition("2024")
        .crate_type("lib")
        .emit("metadata")
        .extern_("selection", "transitive-signature/libselection.rmeta")
        .run();
}
