use crate::runtest::artifacts::*;

#[test]
fn build_records_preserve_the_complete_command_fingerprint() {
    let record = BuildRecord {
        revision: "bpass1".to_owned(),
        command: CommandFingerprint("--cfg\0feature=\"first\nsecond\"\0λ\0".to_owned()),
    };

    assert_eq!(record.to_string().parse::<BuildRecord>(), Ok(record));
    for record in ["", "bpass1", "bpass1\0command"] {
        assert_eq!(record.parse::<BuildRecord>(), Err(()));
    }
    let record =
        BuildRecord { revision: "bpass1".to_owned(), command: CommandFingerprint(String::new()) };
    assert_eq!(record.to_string().parse::<BuildRecord>(), Ok(record));
}

#[test]
fn command_fingerprints_use_logical_sources_and_ordered_environment_changes() {
    let mut first = Command::new("rustc");
    first.arg("auxiliary/first.rs").arg("--crate-type=rlib");
    first.env("COMPILETEST_B", "two").env_remove("COMPILETEST_A");
    let mut second = Command::new("rustc");
    second.arg("auxiliary/second.rs").arg("--crate-type=rlib");
    second.env_remove("COMPILETEST_A").env("COMPILETEST_B", "two");
    let first = CommandFingerprint::from_command(
        &first,
        Some((Utf8Path::new("auxiliary/first.rs"), Utf8Path::new("auxiliary/provider.rs"))),
    );
    let second_fingerprint = CommandFingerprint::from_command(
        &second,
        Some((Utf8Path::new("auxiliary/second.rs"), Utf8Path::new("auxiliary/provider.rs"))),
    );
    assert_eq!(first, second_fingerprint);
    assert_ne!(first, CommandFingerprint::from_command(&second, None));

    second.env("COMPILETEST_A", "one");
    assert_ne!(
        first,
        CommandFingerprint::from_command(
            &second,
            Some((Utf8Path::new("auxiliary/second.rs"), Utf8Path::new("auxiliary/provider.rs"))),
        )
    );
    second.env_remove("COMPILETEST_A").arg("--cfg=changed");
    assert_ne!(
        first,
        CommandFingerprint::from_command(
            &second,
            Some((Utf8Path::new("auxiliary/second.rs"), Utf8Path::new("auxiliary/provider.rs"))),
        )
    );
}

#[test]
fn reuse_requires_a_matching_record_from_the_immediate_predecessor() {
    let command = CommandFingerprint("--crate-type=rlib\0".to_owned());
    for (record, current_revision, previous_revision) in [
        (None, Some("bpass2"), Some("bpass1")),
        (Some("malformed"), Some("bpass2"), Some("bpass1")),
        (Some("bpass0\n--crate-type=rlib\0"), Some("bpass2"), Some("bpass1")),
        (Some("bpass2\n--crate-type=rlib\0"), Some("bpass2"), Some("bpass1")),
        (Some("bpass1\n--crate-type=dylib\0"), Some("bpass2"), Some("bpass1")),
        (Some("bpass1\n--crate-type=rlib\0"), Some("bpass1"), None),
        (Some("bpass1\n--crate-type=rlib\0"), None, Some("bpass1")),
    ] {
        let plan = BuildPlan::parse(
            record.and_then(|record| record.parse().ok()),
            "build/consumer.o: build/liba.rmeta\n".parse().unwrap(),
            &ProviderDeltas::default(),
            ReuseRequest { current_revision, previous_revision, command: &command, inputs: None },
        )
        .unwrap();
        match plan {
            BuildPlan::Invoke => {}
            BuildPlan::Reuse(_) => panic!(),
        }
    }
}

#[test]
fn reuse_advances_the_record_across_consecutive_skipped_revisions() {
    let command = CommandFingerprint("--crate-type=rlib\0".to_owned());
    let mut record = BuildRecord { revision: "bpass1".to_owned(), command: command.clone() };
    for (current, previous) in [("bpass2", "bpass1"), ("bpass3", "bpass2")] {
        let plan = BuildPlan::parse(
            Some(record),
            "build/consumer.o: build/liba.rmeta\n".parse().unwrap(),
            &ProviderDeltas::default(),
            ReuseRequest {
                current_revision: Some(current),
                previous_revision: Some(previous),
                command: &command,
                inputs: None,
            },
        )
        .unwrap();
        let BuildPlan::Reuse(plan) = plan else { panic!() };
        record = plan.into_record();
        assert_eq!(record, BuildRecord { revision: current.to_owned(), command: command.clone() });
        record = record.to_string().parse().unwrap();
    }
}

#[test]
fn reuse_rejects_observed_changes_and_unmeasured_providers() {
    let command = CommandFingerprint("--crate-type=rlib\0".to_owned());
    for delta in [
        ProviderDelta::Measured(FxHashSet::from_iter([ArtifactPath::Rmeta(Utf8PathBuf::from(
            "build/liba.rmeta",
        ))])),
        ProviderDelta::Unmeasured,
    ] {
        let plan = BuildPlan::parse(
            Some(BuildRecord { revision: "bpass1".to_owned(), command: command.clone() }),
            "build/consumer.o: build/liba.rmeta\n".parse().unwrap(),
            &ProviderDeltas(FxHashMap::from_iter([(
                ArtifactProvider {
                    source: Utf8PathBuf::from("auxiliary/a.rs"),
                    out_dir: Utf8PathBuf::from("build"),
                },
                delta.clone(),
            )])),
            ReuseRequest {
                current_revision: Some("bpass2"),
                previous_revision: Some("bpass1"),
                command: &command,
                inputs: None,
            },
        );
        match (plan, delta) {
            (Ok(BuildPlan::Invoke), ProviderDelta::Measured(_)) => {}
            (Err(error), ProviderDelta::Unmeasured) => assert!(error.contains("auxiliary/a.rs")),
            (Ok(BuildPlan::Reuse(_)), _)
            | (Ok(BuildPlan::Invoke), ProviderDelta::Unmeasured)
            | (Err(_), ProviderDelta::Measured(_)) => {
                panic!()
            }
        }
    }
}

#[test]
fn reuse_compares_logical_revision_sources_and_other_own_inputs() {
    enum InputChange {
        None,
        Root,
        Sibling,
        MissingSource,
        MissingSnapshot,
    }

    let root = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .unwrap()
        .join(format!("compiletest-reuse-inputs-{}", std::process::id()));
    let source_root = root.join("sources");
    let snapshot_root = root.join("snapshots");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&snapshot_root).unwrap();
    fs::write(source_root.join("first.rs"), b"source").unwrap();
    let crate_source = source_root.join("a.rs");
    let input_path = source_root.join("second.rs");
    let candidates = vec![Utf8PathBuf::from("first.rs"), Utf8PathBuf::from("second.rs")];
    let command = CommandFingerprint("a.rs\0".to_owned());
    for change in [
        InputChange::None,
        InputChange::Root,
        InputChange::Sibling,
        InputChange::MissingSource,
        InputChange::MissingSnapshot,
    ] {
        fs::write(&input_path, b"source").unwrap();
        fs::write(source_root.join("sibling.rs"), b"sibling").unwrap();
        fs::write(snapshot_root.join("a.rs"), b"source").unwrap();
        fs::write(snapshot_root.join("sibling.rs"), b"sibling").unwrap();
        match change {
            InputChange::None => {}
            InputChange::Root => fs::write(&input_path, b"changed").unwrap(),
            InputChange::Sibling => fs::write(source_root.join("sibling.rs"), b"changed").unwrap(),
            InputChange::MissingSource => fs::remove_file(&input_path).unwrap(),
            InputChange::MissingSnapshot => fs::remove_file(snapshot_root.join("a.rs")).unwrap(),
        }
        let plan = BuildPlan::parse(
            Some(BuildRecord { revision: "bpass1".to_owned(), command: command.clone() }),
            format!(
                "build/liba.rmeta: {} {}\n",
                source_root.join("first.rs"),
                source_root.join("sibling.rs"),
            )
            .parse()
            .unwrap(),
            &ProviderDeltas::default(),
            ReuseRequest {
                current_revision: Some("bpass2"),
                previous_revision: Some("bpass1"),
                command: &command,
                inputs: Some(RevisionInputs {
                    source_root: &source_root,
                    revision_source_candidates: &candidates,
                    crate_source: &crate_source,
                    input_path: &input_path,
                    snapshot_root: &snapshot_root,
                }),
            },
        );
        match (plan, change) {
            (Ok(BuildPlan::Reuse(plan)), InputChange::None) => {
                assert_eq!(
                    plan.into_record(),
                    BuildRecord { revision: "bpass2".to_owned(), command: command.clone() }
                );
            }
            (Err(error), InputChange::Sibling) => assert!(error.contains("`sibling.rs`")),
            (Err(error), InputChange::Root) => assert!(error.contains("`a.rs`")),
            (Err(error), InputChange::MissingSource) => {
                assert!(error.contains(input_path.as_str()));
            }
            (Err(error), InputChange::MissingSnapshot) => {
                assert!(error.contains(snapshot_root.join("a.rs").as_str()));
            }
            _ => panic!(),
        }
    }
    fs::remove_dir_all(root).unwrap();
}

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
