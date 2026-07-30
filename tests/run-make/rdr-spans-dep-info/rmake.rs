//@ needs-target-std

use run_make_support::{rfs, rust_lib_name, rustc};

fn main() {
    let a_rlib = rust_lib_name("a");
    rustc().input("a.rs").crate_name("a").crate_type("rlib").arg("-Zrdr").run();

    let a_spans = std::path::Path::new(&a_rlib).with_extension("spans");

    rustc()
        .input("semantic.rs")
        .crate_name("semantic")
        .crate_type("rlib")
        .edition("2024")
        .extern_("a", &a_rlib)
        .arg("-Zrdr")
        .arg("-Zbinary-dep-depinfo")
        .arg("--emit=metadata,dep-info=semantic.d")
        .run();
    let semantic_dep_info = rfs::read_to_string("semantic.d");
    assert!(semantic_dep_info.contains(&a_rlib));
    assert!(!semantic_dep_info.contains(&a_spans.display().to_string()));

    rustc()
        .input("position.rs")
        .crate_name("position")
        .crate_type("rlib")
        .extern_("a", &a_rlib)
        .arg("-Zrdr")
        .arg("-Zbinary-dep-depinfo")
        .arg("-Cdebuginfo=2")
        .arg("--emit=link,dep-info=position.d")
        .run();
    assert!(rfs::read_to_string("position.d").contains(&a_spans.display().to_string()));
}
