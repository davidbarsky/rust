//@ ignore-cross-compile
//@ needs-symlink

use run_make_support::{path, rfs, rustc};

fn main() {
    rustc()
        .input("a.rs")
        .crate_name("a")
        .crate_type("rlib")
        .arg("-Zrdr")
        .emit("metadata")
        .output("liba.rmeta")
        .run();

    rfs::create_dir_all("cas");
    rfs::create_dir_all("stage");
    rfs::rename("liba.rmeta", "cas/primary-blob");
    rfs::rename("liba.spans", "cas/spans-blob");
    rfs::symlink_file(path("cas/primary-blob"), path("stage/liba.rmeta"));
    rfs::symlink_file(path("cas/spans-blob"), path("stage/liba.spans"));

    rustc()
        .input("b.rs")
        .crate_name("b")
        .crate_type("rlib")
        .extern_("a", "stage/liba.rmeta")
        .emit("metadata")
        .run();
}
