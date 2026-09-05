use crate::runtest::artifacts::*;

#[test]
fn provider_deltas_union_exact_artifacts_under_their_provider() {
    let mut deltas = ProviderDeltas::default();

    deltas.extend([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/liba.rmeta",
        ))])),
    )]);
    deltas.extend([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Link(Utf8PathBuf::from(
            "build/liba.rlib",
        ))])),
    )]);
    deltas.extend([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/b.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/libb.rmeta",
        ))])),
    )]);

    assert_eq!(
        deltas,
        ProviderDeltas(FxHashMap::from_iter([
            (
                ArtifactProvider {
                    source: Utf8PathBuf::from("auxiliary/a.rs"),
                    out_dir: Utf8PathBuf::from("build")
                },
                ProviderDelta::Measured(FxHashSet::from_iter([
                    ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")),
                    ArtifactPath::Link(Utf8PathBuf::from("build/liba.rlib")),
                ])),
            ),
            (
                ArtifactProvider {
                    source: Utf8PathBuf::from("auxiliary/b.rs"),
                    out_dir: Utf8PathBuf::from("build")
                },
                ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(
                    Utf8PathBuf::from("build/libb.rmeta")
                )])),
            ),
        ]))
    );
}

#[test]
fn an_unmeasured_provider_poisons_the_union() {
    let mut deltas = ProviderDeltas::default();
    deltas.extend([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/liba.rmeta",
        ))])),
    )]);
    deltas.extend([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Unmeasured,
    )]);

    assert_eq!(
        deltas,
        ProviderDeltas(FxHashMap::from_iter([(
            ArtifactProvider {
                source: Utf8PathBuf::from("auxiliary/a.rs"),
                out_dir: Utf8PathBuf::from("build")
            },
            ProviderDelta::Unmeasured
        )]))
    );
    let dep_info: MakeDepInfo = "build/consumer.o: build/libb.rmeta\n".parse().unwrap();
    assert!(should_rerun(&deltas, &dep_info).unwrap_err().contains("auxiliary/a.rs"));
}

#[test]
fn artifact_intersection_rejects_substring_matches() {
    let deltas = ProviderDeltas(FxHashMap::from_iter([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/liba.rmeta",
        ))])),
    )]));
    let dep_info: MakeDepInfo =
        "build/consumer.o: build/liba.rmetadata build/nested/liba.rmeta\n".parse().unwrap();

    assert_eq!(should_rerun(&deltas, &dep_info), Ok(false));
}

#[test]
fn unchanged_sibling_artifacts_are_not_deltas() {
    let previous = ArtifactSnapshot(FxHashMap::from_iter([
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")), b"a before".to_vec()),
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libb.rmeta")), b"b".to_vec()),
    ]));
    let current = ArtifactSnapshot(FxHashMap::from_iter([
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta")), b"a after".to_vec()),
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libb.rmeta")), b"b".to_vec()),
    ]));

    assert_eq!(
        current.changed_paths_since(&previous),
        FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta"))])
    );
}

#[test]
fn artifact_additions_and_removals_are_named_path_deltas() {
    let previous = ArtifactSnapshot(FxHashMap::from_iter([
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libremoved.rmeta")), b"removed".to_vec()),
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libstable.rmeta")), b"stable".to_vec()),
    ]));
    let current = ArtifactSnapshot(FxHashMap::from_iter([
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libadded.rmeta")), b"added".to_vec()),
        (ArtifactPath::Rmeta(Utf8PathBuf::from("build/libstable.rmeta")), b"stable".to_vec()),
    ]));

    assert_eq!(
        current.changed_paths_since(&previous),
        FxHashSet::from_iter([
            ArtifactPath::Rmeta(Utf8PathBuf::from("build/libadded.rmeta")),
            ArtifactPath::Rmeta(Utf8PathBuf::from("build/libremoved.rmeta")),
        ])
    );
}

#[test]
fn unobserved_artifact_does_not_schedule_consumer() {
    let deltas = ProviderDeltas(FxHashMap::from_iter([(
        ArtifactProvider {
            source: Utf8PathBuf::from("auxiliary/a.rs"),
            out_dir: Utf8PathBuf::from("build"),
        },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/liba.rmeta",
        ))])),
    )]));
    let dep_info: MakeDepInfo = "build/consumer.o: build/libb.rmeta\n".parse().unwrap();

    assert_eq!(should_rerun(&deltas, &dep_info), Ok(false));
}

#[test]
fn c_observes_a_owned_transitive_artifacts_without_b_observing_them() {
    let deltas = ProviderDeltas(FxHashMap::from_iter([
        (
            ArtifactProvider {
                source: Utf8PathBuf::from("auxiliary/a.rs"),
                out_dir: Utf8PathBuf::from("build"),
            },
            ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Link(Utf8PathBuf::from(
                "build/liba.rlib",
            ))])),
        ),
        (
            ArtifactProvider {
                source: Utf8PathBuf::from("auxiliary/b.rs"),
                out_dir: Utf8PathBuf::from("build"),
            },
            ProviderDelta::Measured(FxHashSet::default()),
        ),
    ]));
    let b_dep_info: MakeDepInfo = "build/libb.rmeta: build/liba.rmeta\n".parse().unwrap();
    let c_dep_info: MakeDepInfo =
        "build/consumer.o: build/liba.rlib build/libb.rmeta\n".parse().unwrap();

    assert_eq!(should_rerun(&deltas, &b_dep_info), Ok(false));
    assert_eq!(should_rerun(&deltas, &c_dep_info), Ok(true));
}

#[test]
fn byte_expectations_require_presence_in_both_snapshots() {
    let rmeta = ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta"));
    let empty = ArtifactSnapshot::default();
    let populated = ArtifactSnapshot(FxHashMap::from_iter([(rmeta.clone(), b"a".to_vec())]));

    for (previous, current) in [(&empty, &populated), (&populated, &empty), (&empty, &empty)] {
        for expectation in [ByteExpectation::Same, ByteExpectation::Different] {
            let result =
                check_byte_expectations(&[(rmeta.clone(), expectation)], previous, current);
            assert_eq!(
                result,
                Err("artifact `build/liba.rmeta` must exist in both the previous and current \
                     snapshots"
                    .to_owned())
            );
        }
    }
}

#[test]
fn byte_expectations_compare_snapshot_bytes() {
    let rmeta = ArtifactPath::Rmeta(Utf8PathBuf::from("build/liba.rmeta"));
    let before = ArtifactSnapshot(FxHashMap::from_iter([(rmeta.clone(), b"before".to_vec())]));
    let after = ArtifactSnapshot(FxHashMap::from_iter([(rmeta.clone(), b"after".to_vec())]));

    assert_eq!(
        check_byte_expectations(&[(rmeta.clone(), ByteExpectation::Same)], &before, &before),
        Ok(())
    );
    assert_eq!(
        check_byte_expectations(&[(rmeta.clone(), ByteExpectation::Different)], &before, &after),
        Ok(())
    );
    assert_eq!(
        check_byte_expectations(&[(rmeta.clone(), ByteExpectation::Different)], &before, &before),
        Err("expected artifact bytes to change: `build/liba.rmeta`".to_owned())
    );
    assert_eq!(
        check_byte_expectations(&[(rmeta.clone(), ByteExpectation::Same)], &before, &after),
        Err("expected artifact bytes to be unchanged: `build/liba.rmeta`".to_owned())
    );
}

#[test]
fn physical_revision_sources_share_one_logical_source_name() {
    let source_root = Utf8Path::new("/tests/incremental/case/auxiliary");
    let crate_source = source_root.join("a.rs");
    let candidates = vec![Utf8PathBuf::from("a_first.rs"), Utf8PathBuf::from("a_second.rs")];
    let dep_info: MakeDepInfo = "/build/liba.rmeta: /tests/incremental/case/auxiliary/a_second.rs \
        /tests/incremental/case/auxiliary/sibling.rs \
        /tests/incremental/case/auxiliary/nested/leaf.rs /library/std/src/lib.rs\n"
        .parse()
        .unwrap();

    let layout = logical_source_layout(&dep_info, source_root, &candidates, &crate_source).unwrap();

    assert_eq!(
        layout,
        FxIndexMap::from_iter([
            (Utf8PathBuf::from("a.rs"), source_root.join("a_second.rs")),
            (Utf8PathBuf::from("sibling.rs"), source_root.join("sibling.rs")),
            (Utf8PathBuf::from("nested/leaf.rs"), source_root.join("nested/leaf.rs")),
        ])
    );
}

#[test]
fn inputs_that_escape_the_source_root_are_rejected() {
    let source_root = Utf8Path::new("/tests/incremental/case/auxiliary");
    let crate_source = source_root.join("a.rs");
    let dep_info: MakeDepInfo = "/build/liba.rmeta: /tests/incremental/case/auxiliary/a.rs \
        /tests/incremental/case/auxiliary/../escaped.rs\n"
        .parse()
        .unwrap();

    let error = logical_source_layout(&dep_info, source_root, &[], &crate_source).unwrap_err();

    assert!(error.contains("does not stay inside"), "{error}");
}

#[test]
fn a_layout_without_the_crate_root_is_rejected() {
    let source_root = Utf8Path::new("/tests/incremental/case/auxiliary");
    let crate_source = source_root.join("a.rs");
    let dep_info: MakeDepInfo =
        "/build/liba.rmeta: /tests/incremental/case/auxiliary/sibling.rs\n".parse().unwrap();

    let error = logical_source_layout(&dep_info, source_root, &[], &crate_source).unwrap_err();

    assert!(error.contains("does not list its root input"), "{error}");
}

#[test]
fn inputs_that_share_a_logical_name_are_rejected() {
    let source_root = Utf8Path::new("/tests/incremental/case/auxiliary");
    let crate_source = source_root.join("a.rs");
    let candidates = vec![Utf8PathBuf::from("a_first.rs")];
    let dep_info: MakeDepInfo = "/build/liba.rmeta: /tests/incremental/case/auxiliary/a_first.rs \
        /tests/incremental/case/auxiliary/a.rs\n"
        .parse()
        .unwrap();

    let error =
        logical_source_layout(&dep_info, source_root, &candidates, &crate_source).unwrap_err();

    assert!(error.ends_with("both stage as `a.rs`"), "{error}");
}

#[cfg(unix)]
#[test]
fn an_artifact_observed_through_a_symlink_schedules_the_consumer() {
    let root = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .unwrap()
        .join(format!("compiletest-artifact-identity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("real")).unwrap();
    std::fs::write(root.join("real/liba.rmeta"), b"a").unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let link = root.join("link");
    let real = root.join("real").canonicalize_utf8().unwrap();

    let deltas = ProviderDeltas(FxHashMap::from_iter([(
        ArtifactProvider { source: Utf8PathBuf::from("auxiliary/a.rs"), out_dir: link.clone() },
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(
            link.join("liba.rmeta"),
        )])),
    )]));
    let dep_info: MakeDepInfo =
        format!("{}: {}\n", real.join("consumer.o"), real.join("liba.rmeta")).parse().unwrap();
    let rerun = should_rerun(&deltas, &dep_info);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(rerun, Ok(true));
}
