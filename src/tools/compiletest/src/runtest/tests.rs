use super::*;

#[test]
fn provider_deltas_union_exact_artifacts_under_their_provider() {
    let a = ArtifactProvider(Utf8PathBuf::from("auxiliary/a.rs"));
    let b = ArtifactProvider(Utf8PathBuf::from("auxiliary/b.rs"));
    let mut deltas = ProviderDeltas::default();

    deltas.extend([(
        a.clone(),
        HashSet::from([
            ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")),
            ArtifactPath::Spans(Utf8PathBuf::from("build/liba.spans")),
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/a.rs")),
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/sibling.rs")),
        ]),
    )]);
    deltas.extend([(
        b.clone(),
        HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
            "build/libb.source-bundle/src/b.rs",
        ))]),
    )]);

    assert_eq!(
        deltas,
        ProviderDeltas(HashMap::from([
            (
                a,
                HashSet::from([
                    ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")),
                    ArtifactPath::Spans(Utf8PathBuf::from("build/liba.spans")),
                    ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/a.rs",)),
                    ArtifactPath::Source(Utf8PathBuf::from(
                        "build/liba.source-bundle/src/sibling.rs",
                    )),
                ]),
            ),
            (
                b,
                HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
                    "build/libb.source-bundle/src/b.rs",
                ))]),
            ),
        ]))
    );
}

#[test]
fn make_dep_info_parses_escaped_spaces_and_continuations_as_exact_paths() {
    let dep_info: MakeDepInfo =
        r"build/consumer\ output.o: build/liba.rmeta build/liba.source-bundle/src/with\ space.rs \
 build/liba.spans
"
        .parse()
        .unwrap();

    assert_eq!(
        dep_info,
        MakeDepInfo(HashSet::from([
            Utf8PathBuf::from("build/liba.rmeta"),
            Utf8PathBuf::from("build/liba.source-bundle/src/with space.rs"),
            Utf8PathBuf::from("build/liba.spans"),
        ]))
    );
}

#[test]
fn artifact_intersection_rejects_substring_matches() {
    let deltas = ProviderDeltas(HashMap::from([(
        ArtifactProvider(Utf8PathBuf::from("auxiliary/a.rs")),
        HashSet::from([
            ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")),
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/leaf.rs")),
        ]),
    )]));
    let dep_info: MakeDepInfo = "build/consumer.o: build/liba.rmetadata \
        build/liba.source-bundle/src/leaf.rs.backup\n"
        .parse()
        .unwrap();

    assert!(!should_rerun(&deltas, &dep_info));
}

#[test]
fn sibling_source_leaves_change_independently() {
    let previous = ArtifactSnapshot(HashMap::from([
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/left.rs")),
            b"left before".to_vec(),
        ),
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/right.rs")),
            b"right".to_vec(),
        ),
    ]));
    let current = ArtifactSnapshot(HashMap::from([
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/left.rs")),
            b"left after".to_vec(),
        ),
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/right.rs")),
            b"right".to_vec(),
        ),
    ]));

    assert_eq!(
        current.changed_paths_since(&previous),
        HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
            "build/liba.source-bundle/src/left.rs",
        ))])
    );
}

#[test]
fn source_additions_and_removals_are_named_path_deltas() {
    let previous = ArtifactSnapshot(HashMap::from([
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/removed.rs")),
            b"removed".to_vec(),
        ),
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/stable.rs")),
            b"stable".to_vec(),
        ),
    ]));
    let current = ArtifactSnapshot(HashMap::from([
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/added.rs")),
            b"added".to_vec(),
        ),
        (
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/stable.rs")),
            b"stable".to_vec(),
        ),
    ]));

    assert_eq!(
        current.changed_paths_since(&previous),
        HashSet::from([
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/added.rs",)),
            ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/removed.rs",)),
        ])
    );
}

#[test]
fn unobserved_source_child_does_not_schedule_consumer() {
    let deltas = ProviderDeltas(HashMap::from([(
        ArtifactProvider(Utf8PathBuf::from("auxiliary/a.rs")),
        HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
            "build/liba.source-bundle/src/unobserved.rs",
        ))]),
    )]));
    let dep_info: MakeDepInfo =
        "build/consumer.o: build/liba.source-bundle/src/observed.rs\n".parse().unwrap();

    assert!(!should_rerun(&deltas, &dep_info));
}

#[test]
fn rerun_replaces_only_its_providers_own_delta() {
    let a = ArtifactProvider(Utf8PathBuf::from("auxiliary/a.rs"));
    let b = ArtifactProvider(Utf8PathBuf::from("auxiliary/b.rs"));
    let mut deltas = ProviderDeltas(HashMap::from([
        (a.clone(), HashSet::from([ArtifactPath::Spans(Utf8PathBuf::from("build/liba.spans"))])),
        (b.clone(), HashSet::from([ArtifactPath::Rmeta(Utf8PathBuf::from("build/libb.rmeta"))])),
    ]));

    deltas.replace(
        b.clone(),
        HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
            "build/libb.source-bundle/src/b.rs",
        ))]),
    );

    assert_eq!(
        deltas,
        ProviderDeltas(HashMap::from([
            (a, HashSet::from([ArtifactPath::Spans(Utf8PathBuf::from("build/liba.spans"))]),),
            (
                b,
                HashSet::from([ArtifactPath::Source(Utf8PathBuf::from(
                    "build/libb.source-bundle/src/b.rs",
                ))]),
            ),
        ]))
    );
}

#[test]
fn c_observes_a_owned_transitive_artifacts_without_b_observing_them() {
    let deltas = ProviderDeltas(HashMap::from([
        (
            ArtifactProvider(Utf8PathBuf::from("auxiliary/a.rs")),
            HashSet::from([
                ArtifactPath::Spans(Utf8PathBuf::from("build/liba.spans")),
                ArtifactPath::Source(Utf8PathBuf::from("build/liba.source-bundle/src/a.rs")),
            ]),
        ),
        (ArtifactProvider(Utf8PathBuf::from("auxiliary/b.rs")), HashSet::new()),
    ]));
    let b_dep_info: MakeDepInfo = "build/libb.rmeta: build/liba.rmeta\n".parse().unwrap();
    let c_spans_dep_info: MakeDepInfo =
        "build/consumer.o: build/liba.spans build/libb.rmeta\n".parse().unwrap();
    let c_source_dep_info: MakeDepInfo =
        "build/consumer.o: build/liba.source-bundle/src/a.rs build/libb.rmeta\n".parse().unwrap();

    assert!(!should_rerun(&deltas, &b_dep_info));
    assert!(should_rerun(&deltas, &c_spans_dep_info));
    assert!(should_rerun(&deltas, &c_source_dep_info));
}

#[test]
fn normalize_platform_differences() {
    assert_eq!(TestCx::normalize_platform_differences(r"$DIR\foo.rs"), "$DIR/foo.rs");
    assert_eq!(
        TestCx::normalize_platform_differences(r"$BUILD_DIR\..\parser.rs"),
        "$BUILD_DIR/../parser.rs"
    );
    assert_eq!(
        TestCx::normalize_platform_differences(r"$DIR\bar.rs: hello\nworld"),
        r"$DIR/bar.rs: hello\nworld"
    );
    assert_eq!(
        TestCx::normalize_platform_differences(r"either bar\baz.rs or bar\baz\mod.rs"),
        r"either bar/baz.rs or bar/baz/mod.rs",
    );
    assert_eq!(TestCx::normalize_platform_differences(r"`.\some\path.rs`"), r"`./some/path.rs`",);
    assert_eq!(TestCx::normalize_platform_differences(r"`some\path.rs`"), r"`some/path.rs`",);
    assert_eq!(
        TestCx::normalize_platform_differences(r"$DIR\path-with-dashes.rs"),
        r"$DIR/path-with-dashes.rs"
    );
    assert_eq!(
        TestCx::normalize_platform_differences(r"$DIR\path_with_underscores.rs"),
        r"$DIR/path_with_underscores.rs",
    );
    assert_eq!(TestCx::normalize_platform_differences(r"$DIR\foo.rs:12:11"), "$DIR/foo.rs:12:11",);
    assert_eq!(
        TestCx::normalize_platform_differences(r"$DIR\path with\spaces 'n' quotes"),
        "$DIR/path with/spaces 'n' quotes",
    );
    assert_eq!(
        TestCx::normalize_platform_differences(r"$DIR\file_with\no_extension"),
        "$DIR/file_with/no_extension",
    );

    assert_eq!(TestCx::normalize_platform_differences(r"\n"), r"\n");
    assert_eq!(TestCx::normalize_platform_differences(r"{ \n"), r"{ \n");
    assert_eq!(TestCx::normalize_platform_differences(r"`\]`"), r"`\]`");
    assert_eq!(TestCx::normalize_platform_differences(r#""\{""#), r#""\{""#);
    assert_eq!(
        TestCx::normalize_platform_differences(r#"write!(&mut v, "Hello\n")"#),
        r#"write!(&mut v, "Hello\n")"#
    );
    assert_eq!(
        TestCx::normalize_platform_differences(r#"println!("test\ntest")"#),
        r#"println!("test\ntest")"#,
    );
}
