use camino::Utf8PathBuf;

use crate::dep_info::{FxIndexSet, MakeDepInfo};

#[test]
fn make_dep_info_keeps_unique_utf8_paths_in_encounter_order() {
    let dep_info: MakeDepInfo =
        "build/a.o: src/東京.rs src/a.rs src/東京.rs\nbuild/a.d: src/a.rs src/café.rs\n"
            .parse()
            .unwrap();

    assert_eq!(
        dep_info.0.into_iter().collect::<Vec<_>>(),
        vec![
            Utf8PathBuf::from("src/東京.rs"),
            Utf8PathBuf::from("src/a.rs"),
            Utf8PathBuf::from("src/café.rs"),
        ]
    );
}

#[test]
fn make_dep_info_parses_escaped_spaces_and_continuations_as_exact_paths() {
    let dep_info: MakeDepInfo =
        r"build/consumer\ output.o: build/liba.rmeta build/nested\ dir/libb.rmeta \
 build/libc.rmeta
"
        .parse()
        .unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(FxIndexSet::from_iter([
            Utf8PathBuf::from("build/liba.rmeta"),
            Utf8PathBuf::from("build/nested dir/libb.rmeta"),
            Utf8PathBuf::from("build/libc.rmeta"),
        ]))
    );
}

#[test]
fn make_dep_info_keeps_hash_and_dollar_inside_paths() {
    let dep_info: MakeDepInfo = "build/a.o: src/has#hash.rs src/has$dollar.rs\n".parse().unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(FxIndexSet::from_iter([
            Utf8PathBuf::from("src/has#hash.rs"),
            Utf8PathBuf::from("src/has$dollar.rs"),
        ]))
    );
}

#[test]
fn make_dep_info_ignores_target_only_rules_and_env_dep_comments() {
    let dep_info: MakeDepInfo =
        "build/a.d: src/lib.rs src/module.rs\n\nsrc/lib.rs:\nsrc/module.rs:\n\n# env-dep:KEY=value\n"
            .parse()
            .unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(FxIndexSet::from_iter([
            Utf8PathBuf::from("src/lib.rs"),
            Utf8PathBuf::from("src/module.rs"),
        ]))
    );
}

#[test]
fn make_dep_info_accepts_crlf_line_endings_and_continuations() {
    let dep_info: MakeDepInfo = "build/a.o: src/lib.rs \\\r\n src/module.rs\r\n".parse().unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(FxIndexSet::from_iter([
            Utf8PathBuf::from("src/lib.rs"),
            Utf8PathBuf::from("src/module.rs"),
        ]))
    );
}

#[test]
fn make_dep_info_separates_windows_targets_from_verbatim_prerequisites() {
    let dep_info: MakeDepInfo =
        "C:\\build\\a.o: \\\\?\\C:\\src\\lib.rs C:\\build\\liba.rmeta\n".parse().unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(FxIndexSet::from_iter([
            Utf8PathBuf::from("\\\\?\\C:\\src\\lib.rs"),
            Utf8PathBuf::from("C:\\build\\liba.rmeta"),
        ]))
    );
}
