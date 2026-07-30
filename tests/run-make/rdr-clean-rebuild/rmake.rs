//@ ignore-cross-compile
//@ needs-target-std

use run_make_support::{rfs, run, rustc};

fn main() {
    for (source, output) in [
        ("a_first.rs", "first"),
        ("a_body.rs", "body"),
        ("a_add.rs", "add"),
        ("a_remove.rs", "remove"),
        ("a_reorder.rs", "reorder"),
        ("a_position.rs", "position"),
    ] {
        rfs::copy(source, "a.rs");
        rfs::create_dir(output);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .arg("-Zrdr")
            .emit("link,metadata")
            .out_dir(output)
            .run();
    }

    let first_rmeta = rfs::read("first/liba.rmeta");
    for output in ["body", "add", "remove", "reorder", "position"] {
        assert!(
            rfs::read(format!("{output}/liba.rmeta")) == first_rmeta,
            "{output}/liba.rmeta changed"
        );
    }
    assert_ne!(rfs::read("reorder/liba.spans"), rfs::read("position/liba.spans"));

    for (source, output) in [
        ("a_closure_first.rs", "metadata-first"),
        ("a_closure_body.rs", "metadata-closure"),
        ("a_closure_nested_fn.rs", "metadata-nested-fn"),
    ] {
        rfs::copy(source, "a.rs");
        rfs::create_dir(output);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .edition("2024")
            .arg("-Zrdr")
            .emit("metadata")
            .out_dir(output)
            .run();
    }
    let metadata_first = rfs::read("metadata-first/liba.rmeta");
    for output in ["metadata-closure", "metadata-nested-fn"] {
        assert!(
            rfs::read(format!("{output}/liba.rmeta")) == metadata_first,
            "{output}/liba.rmeta changed after adding an unselected private definition"
        );
    }

    for (source, output) in [
        ("a_doc_links_first.rs", "doc-links-first"),
        ("a_doc_links_private_names.rs", "doc-links-private-names"),
    ] {
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
    assert_eq!(
        rfs::read("doc-links-private-names/liba.rmeta"),
        rfs::read("doc-links-first/liba.rmeta"),
        "private names changed the encoding order of public intra-doc link resolutions"
    );

    for (source, output) in [
        ("a_rpitit_first.rs", "rpitit-first"),
        ("a_rpitit_position.rs", "rpitit-position"),
        ("a_rpitit_inner_position.rs", "rpitit-inner-position"),
        ("a_rpitit_private_defs.rs", "rpitit-private-defs"),
    ] {
        rfs::copy(source, "a.rs");
        rfs::create_dir(output);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .edition("2024")
            .arg("-Zrdr")
            .emit("metadata")
            .out_dir(output)
            .run();
    }
    assert_eq!(
        rfs::read("rpitit-position/liba.rmeta"),
        rfs::read("rpitit-first/liba.rmeta"),
        "source movement changed public RPITIT metadata"
    );
    assert_ne!(
        rfs::read("rpitit-position/liba.spans"),
        rfs::read("rpitit-first/liba.spans"),
        "the RPITIT source-position fixture did not change encoded spans"
    );
    assert_eq!(
        rfs::read("rpitit-inner-position/liba.rmeta"),
        rfs::read("rpitit-first/liba.rmeta"),
        "source movement inside public RPITIT signatures changed metadata"
    );
    assert_ne!(
        rfs::read("rpitit-inner-position/liba.spans"),
        rfs::read("rpitit-first/liba.spans"),
        "the inner RPITIT source-position fixture did not change encoded spans"
    );
    assert_eq!(
        rfs::read("rpitit-private-defs/liba.rmeta"),
        rfs::read("rpitit-position/liba.rmeta"),
        "private definitions changed the encoding order of public RPITIT maps"
    );

    for (source, output) in [
        ("a_hygiene_first.rs", "hygiene-first"),
        ("a_hygiene_private_expansion.rs", "hygiene-private-expansion"),
    ] {
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
    assert_eq!(
        rfs::read("hygiene-private-expansion/liba.rmeta"),
        rfs::read("hygiene-first/liba.rmeta"),
        "an unselected private expansion changed the encoding of public hygiene data"
    );

    rfs::copy("a_hygiene_first.rs", "a.rs");
    rfs::create_dir("hygiene-incremental-baseline");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("metadata")
        .out_dir("hygiene-incremental-baseline")
        .incremental("hygiene-incremental-cache")
        .run();

    rfs::copy("a_hygiene_private_expansion.rs", "a.rs");
    rfs::create_dir("hygiene-incremental-reused");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("metadata")
        .out_dir("hygiene-incremental-reused")
        .incremental("hygiene-incremental-cache")
        .run();

    rfs::create_dir("hygiene-incremental-fresh");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("metadata")
        .out_dir("hygiene-incremental-fresh")
        .incremental("hygiene-incremental-fresh-cache")
        .run();

    assert_eq!(
        rfs::read("hygiene-incremental-reused/liba.rmeta"),
        rfs::read("hygiene-incremental-fresh/liba.rmeta"),
        "reused metadata encoded public hygiene differently from an empty-cache build"
    );
    assert_eq!(
        rfs::read("hygiene-incremental-reused/liba.spans"),
        rfs::read("hygiene-incremental-fresh/liba.spans"),
        "reused spans addressed hygiene differently from an empty-cache build"
    );

    for (source, output) in [
        ("a_exported_mir_first.rs", "exported-mir-first"),
        ("a_exported_mir_closure.rs", "exported-mir-closure"),
    ] {
        rfs::copy(source, "a.rs");
        rfs::create_dir(output);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .edition("2024")
            .arg("-Zrdr")
            .emit("link,metadata")
            .out_dir(output)
            .run();
    }
    assert_ne!(
        rfs::read("exported-mir-first/liba.rmeta"),
        rfs::read("exported-mir-closure/liba.rmeta"),
        "a closure referenced by exported MIR did not change public metadata"
    );

    for (cache, baseline, edited) in [
        ("incremental-a-cache", "incremental-a-baseline", "incremental-a"),
        ("incremental-b-cache", "incremental-b-baseline", "incremental-b"),
    ] {
        rfs::copy("a_first.rs", "a.rs");
        rfs::create_dir(baseline);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .arg("-Zrdr")
            .emit("link,metadata")
            .out_dir(baseline)
            .incremental(cache)
            .run();

        rfs::copy("a_position.rs", "a.rs");
        rfs::create_dir(edited);
        rustc()
            .input("a.rs")
            .crate_name("a")
            .crate_type("rlib")
            .arg("-Zrdr")
            .emit("link,metadata")
            .out_dir(edited)
            .incremental(cache)
            .run();
    }

    rfs::copy("a_position.rs", "a.rs");
    rfs::create_dir("incremental-fresh");
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("link,metadata")
        .out_dir("incremental-fresh")
        .incremental("incremental-fresh-cache")
        .run();

    let fresh_rmeta = rfs::read("incremental-fresh/liba.rmeta");
    let fresh_spans = rfs::read("incremental-fresh/liba.spans");
    for output in ["incremental-a", "incremental-b"] {
        assert!(
            rfs::read(format!("{output}/liba.rmeta")) == fresh_rmeta,
            "{output}/liba.rmeta differs from the empty-cache build"
        );
        assert!(
            rfs::read(format!("{output}/liba.spans")) == fresh_spans,
            "{output}/liba.spans differs from the empty-cache build"
        );
    }

    rfs::create_dir("downstream");
    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .arg("-Zrdr")
        .extern_("a", "first/liba.rlib")
        .out_dir("downstream")
        .run();

    rustc()
        .input("main.rs")
        .extern_("b", "downstream/libb.rlib")
        .specific_library_search_path("dependency", "position")
        .run();
    run("main");
}
