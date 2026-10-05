use std::panic::{self, AssertUnwindSafe};

use hegel::{TestCase, generators, stateful};
use run_make_support::camino::{Utf8Path, Utf8PathBuf};
use run_make_support::dep_info::MakeDepInfo;
use run_make_support::indexmap::IndexSet;
use run_make_support::regex::{Captures, Regex};
use run_make_support::rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use run_make_support::{CompletedProcess, bin_name, cmd, cwd, rfs, rustc, serde_json, tempfile};

use crate::ledger::{Witness, witness};
use crate::model::{
    CodegenUnits, Crate, CrateKind, DeadCodeLint, Edit, FieldOrder, Inlining, Item, MetadataState,
    Model, Module, Outcome, OverflowChecks, ScaledInstance, Session, Signature, TempFiles, Threads,
    TrackedFlags, Transition, UnstableOptions, UntrackedFlags, Visibility,
};

pub(crate) fn exercise_every_edit_kind() {
    let histories = [
        vec![
            Edit::SetSaveTemps(TempFiles::Kept),
            Edit::BreakBackend(Crate::A),
            Edit::RepairBackend(Crate::A),
        ],
        vec![Edit::SetUnstableOptions(UnstableOptions::Enabled), Edit::SetValue(7)],
        vec![
            Edit::SetGenericFactor(5),
            Edit::SetScaledInstance(ScaledInstance::SharedFromA),
            Edit::SetGenericFactor(6),
            Edit::SetFieldOrder(FieldOrder::SecondFirst),
            Edit::SetLimit(8),
            Edit::SetMacroMultiplier(7),
        ],
        vec![
            Edit::SetCodegenUnits(CodegenUnits::Four),
            Edit::SetThreads(Threads::Two),
            Edit::SetValue(5),
            Edit::SetOverflowChecks(OverflowChecks::Disabled),
            Edit::SetDeadCodeLint(DeadCodeLint::Allow),
            Edit::SetCPrivateMarker(1),
        ],
        vec![
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(1, Module::Root),
            Edit::SetVisibility(1, Visibility::Private),
        ],
        vec![
            Edit::AddItem(2, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(2, Module::Root),
            Edit::MoveItem(2, Module::Inner),
        ],
        vec![
            Edit::AddItem(3, Item { visibility: Visibility::Private, module: Module::Root }),
            Edit::SetVisibility(3, Visibility::Public),
        ],
        vec![
            Edit::AddItem(4, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(4, Module::Root),
            Edit::UnuseItem(4),
        ],
        vec![
            Edit::AddItem(5, Item { visibility: Visibility::Private, module: Module::Root }),
            Edit::SetVisibility(5, Visibility::Public),
            Edit::UseItem(5, Module::Root),
            Edit::SetValue(9),
        ],
        vec![Edit::AddMethod(2), Edit::AddMethod(5), Edit::SwapMethods(2, 5)],
        vec![Edit::AddMethod(0), Edit::SetScaledInstance(ScaledInstance::SharedFromA)],
        vec![
            Edit::SetSignature(Signature::Scaled),
            Edit::SetInline(Inlining::Inline),
            Edit::SetValue(4),
        ],
        vec![
            Edit::AddItem(6, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::RemoveItem(6),
        ],
        vec![
            Edit::SetPadding(2),
            Edit::SetAPrivateMarker(3),
            Edit::SetBPrivateMarker(4),
            Edit::SetCPrivateMarker(5),
            Edit::AddMethod(1),
            Edit::RemoveMethod(1),
        ],
        vec![
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(1, Module::Root),
            Edit::RemoveItem(1),
        ],
        vec![
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(1, Module::Root),
            Edit::SetVisibility(1, Visibility::Private),
            Edit::UnuseItem(1),
        ],
        vec![
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Inner }),
            Edit::UseItem(1, Module::Inner),
            Edit::RemoveItem(1),
            Edit::UnuseItem(1),
        ],
        vec![
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Root }),
            Edit::UseItem(1, Module::Root),
            Edit::RemoveItem(1),
            Edit::AddItem(1, Item { visibility: Visibility::Public, module: Module::Inner }),
            Edit::MoveItem(1, Module::Root),
        ],
        vec![
            Edit::BreakBackend(Crate::B),
            Edit::SetBPrivateMarker(2),
            Edit::RepairBackend(Crate::B),
        ],
        vec![Edit::BreakBackend(Crate::C), Edit::SetValue(2), Edit::RepairBackend(Crate::C)],
        vec![
            Edit::AddItem(7, Item { visibility: Visibility::Private, module: Module::Inner }),
            Edit::MoveItem(7, Module::Root),
        ],
        vec![
            Edit::Batch(vec![Edit::SetValue(6), Edit::AddMethod(3), Edit::SetBPrivateMarker(7)]),
            Edit::Batch(vec![
                Edit::AddItem(8, Item { visibility: Visibility::Public, module: Module::Root }),
                Edit::UseItem(8, Module::Root),
                Edit::SetPadding(1),
            ]),
        ],
    ];
    let covered: FxHashSet<EditKind> = histories.iter().flatten().map(EditKind::of).collect();
    let missing: Vec<EditKind> =
        KINDS.iter().copied().filter(|kind| !covered.contains(kind)).collect();
    assert!(missing.is_empty(), "explicit histories do not cover every edit kind: {missing:?}");
    for krate in Crate::BUILD_ORDER {
        assert!(
            histories.iter().flatten().any(|edit| *edit == Edit::BreakBackend(krate)),
            "explicit histories never break the backend of `{}`",
            krate.name()
        );
    }
    for visibility in [Visibility::Private, Visibility::Public] {
        for module in [Module::Root, Module::Inner] {
            let item = Item { visibility, module };
            assert!(
                histories
                    .iter()
                    .flatten()
                    .any(|edit| matches!(edit, Edit::AddItem(_, added) if *added == item)),
                "explicit histories never add a {item:?}"
            );
        }
    }
    for edits in histories {
        execute_with(None, MirEmission::Emitted, &edits);
    }
}

pub(crate) fn run_generated(tc: TestCase) {
    let mir = tc.draw(generators::default::<MirEmission>());
    let (mut history, baseline) = History::start(None, mir, Model::connected());
    stateful::machine(Exploration { history: &mut history }).steps(6).run(tc);
    history.restore(baseline);
}

struct Exploration<'a> {
    history: &'a mut History,
}

impl Exploration<'_> {
    fn step(&mut self, theme: Theme, tc: TestCase) {
        let kinds: Vec<EditKind> =
            KINDS.iter().copied().filter(|kind| kind.theme() == theme).collect();
        let edit = draw_edit(&kinds, &self.history.model, &tc);
        tc.note(&format!("{edit:?}"));
        self.history.apply(edit);
    }
}

#[hegel::state_machine]
impl Exploration<'_> {
    #[rule(weight = 17)]
    fn api(&mut self, tc: TestCase) {
        self.step(Theme::Api, tc);
    }

    #[rule(weight = 7)]
    fn dispatch(&mut self, tc: TestCase) {
        self.step(Theme::Dispatch, tc);
    }

    #[rule(weight = 8)]
    fn codegen(&mut self, tc: TestCase) {
        self.step(Theme::Codegen, tc);
    }

    #[rule(weight = 4)]
    fn positions(&mut self, tc: TestCase) {
        self.step(Theme::Positions, tc);
    }

    #[rule(weight = 6)]
    fn session(&mut self, tc: TestCase) {
        self.step(Theme::Session, tc);
    }

    #[rule(weight = 4)]
    fn recovery(&mut self, tc: TestCase) {
        self.step(Theme::Recovery, tc);
    }

    #[rule(weight = 2)]
    fn batch(&mut self, tc: TestCase) {
        self.step(Theme::Batch, tc);
    }
}

fn draw_edit(kinds: &[EditKind], model: &Model, tc: &TestCase) -> Edit {
    let mut pool = Vec::new();
    for kind in kinds.iter().copied().filter(|kind| kind.applicable(model)) {
        pool.extend(std::iter::repeat_n(kind, kind.weight()));
    }
    edit_for_kind(tc.draw(generators::sampled_from(pool)), model, tc)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, hegel::DefaultGenerator)]
enum Subject {
    Any,
    UsedByB,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Theme {
    Api,
    Dispatch,
    Codegen,
    Positions,
    Session,
    Recovery,
    Batch,
}

macro_rules! edit_kinds {
    ($($kind:ident($theme:ident, $weight:literal)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        enum EditKind {
            $($kind),*
        }

        const KINDS: &[EditKind] = &[$(EditKind::$kind),*];

        impl EditKind {
            fn theme(self) -> Theme {
                match self {
                    $(EditKind::$kind => Theme::$theme),*
                }
            }

            fn weight(self) -> usize {
                match self {
                    $(EditKind::$kind => $weight),*
                }
            }
        }
    };
}

// Scalar toggles (values, markers, flags) carry weight 1; structural edits that grow or reshape
// the crates carry more, so a bounded history builds up items, methods, and uses instead of only
// flipping scalars.
edit_kinds!(
    Value(Api, 1),
    Signature(Api, 1),
    Inline(Codegen, 1),
    Padding(Positions, 1),
    APrivateMarker(Positions, 1),
    BPrivateMarker(Positions, 1),
    CPrivateMarker(Positions, 1),
    AddItem(Api, 4),
    RemoveItem(Api, 2),
    SetVisibility(Api, 2),
    MoveItem(Api, 2),
    UseItem(Api, 3),
    UnuseItem(Api, 2),
    AddMethod(Dispatch, 3),
    RemoveMethod(Dispatch, 2),
    SwapMethods(Dispatch, 2),
    SaveTemps(Session, 1),
    UnstableOptions(Session, 1),
    CodegenUnits(Session, 1),
    Threads(Session, 1),
    OverflowChecks(Session, 1),
    DeadCodeLint(Session, 1),
    GenericFactor(Codegen, 1),
    ScaledInstance(Codegen, 2),
    FieldOrder(Codegen, 2),
    Limit(Codegen, 1),
    MacroMultiplier(Codegen, 1),
    BreakBackend(Recovery, 2),
    RepairBackend(Recovery, 2),
    Batch(Batch, 2),
);

hegel::pretty_print_as_debug!(EditKind);

impl EditKind {
    /// Whether a `model` in its current state admits an edit of this kind.
    fn applicable(self, model: &Model) -> bool {
        match self {
            EditKind::Value
            | EditKind::Signature
            | EditKind::Inline
            | EditKind::Padding
            | EditKind::APrivateMarker
            | EditKind::BPrivateMarker
            | EditKind::CPrivateMarker
            | EditKind::SaveTemps
            | EditKind::UnstableOptions
            | EditKind::CodegenUnits
            | EditKind::Threads
            | EditKind::OverflowChecks
            | EditKind::DeadCodeLint
            | EditKind::GenericFactor
            | EditKind::ScaledInstance
            | EditKind::FieldOrder
            | EditKind::Limit
            | EditKind::MacroMultiplier
            | EditKind::Batch => true,
            EditKind::AddItem => model.items.len() < 10,
            EditKind::RemoveItem | EditKind::SetVisibility | EditKind::MoveItem => {
                !model.items.is_empty()
            }
            EditKind::UseItem => model.items.iter().any(|(item, value)| {
                value.visibility == Visibility::Public && !model.b_uses.contains_key(item)
            }),
            EditKind::UnuseItem => !model.b_uses.is_empty(),
            EditKind::AddMethod => model.methods.len() < 10,
            EditKind::RemoveMethod => !model.methods.is_empty(),
            EditKind::SwapMethods => model.methods.len() >= 2,
            EditKind::BreakBackend => model.broken_backend.is_none(),
            EditKind::RepairBackend => model.broken_backend.is_some(),
        }
    }

    fn of(edit: &Edit) -> EditKind {
        match edit {
            Edit::SetValue(_) => EditKind::Value,
            Edit::SetSignature(_) => EditKind::Signature,
            Edit::SetInline(_) => EditKind::Inline,
            Edit::SetPadding(_) => EditKind::Padding,
            Edit::SetAPrivateMarker(_) => EditKind::APrivateMarker,
            Edit::SetBPrivateMarker(_) => EditKind::BPrivateMarker,
            Edit::SetCPrivateMarker(_) => EditKind::CPrivateMarker,
            Edit::AddItem(..) => EditKind::AddItem,
            Edit::RemoveItem(_) => EditKind::RemoveItem,
            Edit::SetVisibility(..) => EditKind::SetVisibility,
            Edit::MoveItem(..) => EditKind::MoveItem,
            Edit::UseItem(_, _) => EditKind::UseItem,
            Edit::UnuseItem(_) => EditKind::UnuseItem,
            Edit::AddMethod(_) => EditKind::AddMethod,
            Edit::RemoveMethod(_) => EditKind::RemoveMethod,
            Edit::SwapMethods(..) => EditKind::SwapMethods,
            Edit::SetSaveTemps(_) => EditKind::SaveTemps,
            Edit::SetUnstableOptions(_) => EditKind::UnstableOptions,
            Edit::SetCodegenUnits(_) => EditKind::CodegenUnits,
            Edit::SetThreads(_) => EditKind::Threads,
            Edit::SetOverflowChecks(_) => EditKind::OverflowChecks,
            Edit::SetDeadCodeLint(_) => EditKind::DeadCodeLint,
            Edit::BreakBackend(_) => EditKind::BreakBackend,
            Edit::RepairBackend(_) => EditKind::RepairBackend,
            Edit::SetGenericFactor(_) => EditKind::GenericFactor,
            Edit::SetScaledInstance(_) => EditKind::ScaledInstance,
            Edit::SetFieldOrder(_) => EditKind::FieldOrder,
            Edit::SetLimit(_) => EditKind::Limit,
            Edit::SetMacroMultiplier(_) => EditKind::MacroMultiplier,
            Edit::Batch(_) => EditKind::Batch,
        }
    }
}

fn edit_for_kind(kind: EditKind, model: &Model, tc: &TestCase) -> Edit {
    let other_digit = |current: u32| {
        tc.draw(generators::sampled_from((0..10).filter(|d| *d != current).collect::<Vec<u32>>()))
    };
    let subject = || {
        let present: Vec<u32> = model.items.keys().copied().collect();
        let used: Vec<u32> =
            present.iter().copied().filter(|item| model.b_uses.contains_key(item)).collect();
        let candidates = if used.is_empty() {
            present
        } else {
            match tc.draw(generators::default::<Subject>()) {
                Subject::Any => present,
                Subject::UsedByB => used,
            }
        };
        tc.draw(generators::sampled_from(candidates))
    };
    match kind {
        EditKind::Value => Edit::SetValue(other_digit(model.value)),
        EditKind::Signature => Edit::SetSignature(match model.signature {
            Signature::Plain => Signature::Scaled,
            Signature::Scaled => Signature::Plain,
        }),
        EditKind::Inline => Edit::SetInline(match model.inline {
            Inlining::Default => Inlining::Inline,
            Inlining::Inline => Inlining::Default,
        }),
        EditKind::Padding => Edit::SetPadding(tc.draw(generators::sampled_from(
            (0..4u8).filter(|p| *p != model.padding).collect::<Vec<u8>>(),
        ))),
        EditKind::APrivateMarker => Edit::SetAPrivateMarker(other_digit(model.a_private_marker)),
        EditKind::BPrivateMarker => Edit::SetBPrivateMarker(other_digit(model.b_private_marker)),
        EditKind::CPrivateMarker => Edit::SetCPrivateMarker(other_digit(model.c_private_marker)),
        EditKind::AddItem => {
            let absent: Vec<u32> = (0..10).filter(|item| !model.items.contains_key(item)).collect();
            Edit::AddItem(
                tc.draw(generators::sampled_from(absent)),
                tc.draw(generators::default::<Item>()),
            )
        }
        EditKind::RemoveItem => Edit::RemoveItem(subject()),
        EditKind::SetVisibility => {
            let item = subject();
            match model.items[&item].visibility {
                Visibility::Private => Edit::SetVisibility(item, Visibility::Public),
                Visibility::Public => Edit::SetVisibility(item, Visibility::Private),
            }
        }
        EditKind::MoveItem => {
            let item = subject();
            let to = match model.items[&item].module {
                Module::Root => Module::Inner,
                Module::Inner => Module::Root,
            };
            Edit::MoveItem(item, to)
        }
        EditKind::UseItem => {
            let usable: Vec<u32> = model
                .items
                .iter()
                .filter(|(item, value)| {
                    value.visibility == Visibility::Public && !model.b_uses.contains_key(*item)
                })
                .map(|(item, _)| *item)
                .collect();
            let item = tc.draw(generators::sampled_from(usable));
            Edit::UseItem(item, model.items[&item].module)
        }
        EditKind::UnuseItem => Edit::UnuseItem(
            tc.draw(generators::sampled_from(model.b_uses.keys().copied().collect::<Vec<u32>>())),
        ),
        EditKind::AddMethod => {
            let absent: Vec<u32> = (0..10).filter(|id| !model.methods.contains(id)).collect();
            Edit::AddMethod(tc.draw(generators::sampled_from(absent)))
        }
        EditKind::RemoveMethod => Edit::RemoveMethod(*model.methods.last().unwrap()),
        EditKind::SwapMethods => {
            let first = tc.draw(generators::sampled_from(model.methods.clone()));
            let second = tc.draw(generators::sampled_from(
                model.methods.iter().copied().filter(|id| *id != first).collect::<Vec<u32>>(),
            ));
            Edit::SwapMethods(first, second)
        }
        EditKind::SaveTemps => Edit::SetSaveTemps(match model.session.untracked.save_temps {
            TempFiles::Deleted => TempFiles::Kept,
            TempFiles::Kept => TempFiles::Deleted,
        }),
        EditKind::UnstableOptions => {
            Edit::SetUnstableOptions(match model.session.untracked.unstable_options {
                UnstableOptions::Disabled => UnstableOptions::Enabled,
                UnstableOptions::Enabled => UnstableOptions::Disabled,
            })
        }
        EditKind::CodegenUnits => Edit::SetCodegenUnits(
            tc.draw(generators::sampled_from(
                [CodegenUnits::Default, CodegenUnits::One, CodegenUnits::Four]
                    .into_iter()
                    .filter(|units| *units != model.session.untracked.codegen_units)
                    .collect::<Vec<CodegenUnits>>(),
            )),
        ),
        EditKind::Threads => Edit::SetThreads(match model.session.untracked.threads {
            Threads::Default => Threads::Two,
            Threads::Two => Threads::Default,
        }),
        EditKind::OverflowChecks => {
            Edit::SetOverflowChecks(match model.session.tracked.overflow_checks {
                OverflowChecks::Enabled => OverflowChecks::Disabled,
                OverflowChecks::Disabled => OverflowChecks::Enabled,
            })
        }
        EditKind::DeadCodeLint => Edit::SetDeadCodeLint(match model.session.tracked.dead_code {
            DeadCodeLint::Warn => DeadCodeLint::Allow,
            DeadCodeLint::Allow => DeadCodeLint::Warn,
        }),
        EditKind::GenericFactor => Edit::SetGenericFactor(other_digit(model.generic_factor)),
        EditKind::ScaledInstance => Edit::SetScaledInstance(match model.scaled_instance {
            ScaledInstance::LocalToB => ScaledInstance::SharedFromA,
            ScaledInstance::SharedFromA => ScaledInstance::LocalToB,
        }),
        EditKind::FieldOrder => Edit::SetFieldOrder(match model.field_order {
            FieldOrder::FirstSecond => FieldOrder::SecondFirst,
            FieldOrder::SecondFirst => FieldOrder::FirstSecond,
        }),
        EditKind::Limit => Edit::SetLimit(other_digit(model.limit)),
        EditKind::MacroMultiplier => Edit::SetMacroMultiplier(other_digit(model.macro_multiplier)),
        EditKind::BreakBackend => Edit::BreakBackend(tc.draw(generators::default::<Crate>())),
        EditKind::RepairBackend => Edit::RepairBackend(model.broken_backend.unwrap()),
        EditKind::Batch => {
            let count = tc.draw(generators::integers::<usize>().min_value(2).max_value(3));
            let mut scratch = model.clone();
            let mut edits = Vec::new();
            let kinds: Vec<EditKind> =
                KINDS.iter().copied().filter(|kind| *kind != EditKind::Batch).collect();
            for _ in 0..count {
                let edit = draw_edit(&kinds, &scratch, tc);
                scratch.apply(&edit);
                edits.push(edit);
            }
            Edit::Batch(edits)
        }
    }
}

struct History {
    root: tempfile::TempDir,
    src: Utf8PathBuf,
    incremental: Utf8PathBuf,
    fault: Option<Fault>,
    mir: MirEmission,
    initial: Model,
    model: Model,
    edits: Vec<Edit>,
    inverses: Vec<Edit>,
    last: Option<Transition>,
    sources: FxHashMap<Utf8PathBuf, Vec<u8>>,
    published: FxHashMap<Crate, Published>,
    finalized: FxHashMap<Crate, CrateInputs>,
}

struct Baseline {
    probe: Probe,
    published: FxHashMap<Crate, Published>,
    clean: CleanGraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, hegel::DefaultGenerator)]
enum MirEmission {
    Emitted,
    Omitted,
}

struct CrateInputs {
    tracked: TrackedFlags,
    source: String,
    upstream: Vec<String>,
}

impl CrateInputs {
    fn of(model: &Model, krate: Crate) -> Self {
        CrateInputs {
            tracked: model.session.tracked,
            source: model.render(krate),
            upstream: Crate::BUILD_ORDER[..krate.position()]
                .iter()
                .map(|provider| model.render(*provider))
                .collect(),
        }
    }
}

enum SessionRelation {
    Discarded,
    Loaded(InputDelta),
}

enum InputDelta {
    OwnSource,
    UpstreamOnly,
    Unchanged,
}

impl SessionRelation {
    fn between(finalized: &CrateInputs, current: &CrateInputs) -> Self {
        let CrateInputs {
            tracked: finalized_tracked,
            source: finalized_source,
            upstream: finalized_upstream,
        } = finalized;
        let CrateInputs { tracked, source, upstream } = current;
        if finalized_tracked != tracked {
            return SessionRelation::Discarded;
        }
        let delta = if finalized_source != source {
            InputDelta::OwnSource
        } else if finalized_upstream != upstream {
            InputDelta::UpstreamOnly
        } else {
            InputDelta::Unchanged
        };
        SessionRelation::Loaded(delta)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    RebuildEverything,
    SkipCrate(Crate),
    CompileTwice(Crate),
    BlindToArtifacts,
    BlindToSession,
}

enum Decision {
    Run(Observed),
    Skip(Observed),
}

#[derive(Debug)]
enum Observed {
    Owed,
    Changes(ObservedChanges),
}

#[derive(Debug)]
struct ObservedChanges {
    paths: FxHashSet<Utf8PathBuf>,
    session: Option<(Session, Session)>,
}

impl ObservedChanges {
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.session.is_none()
    }
}

enum CrateStep {
    Blocked,
    Skipped { observed: Observed },
    Built { observed: Observed, diagnostics: Diagnostics },
    Rejected { observed: Observed, diagnostics: Diagnostics },
}

#[derive(Clone)]
struct Published {
    outputs: BuildOutputs,
    session: Session,
    standing: Standing,
}

#[derive(Clone, Copy)]
enum Standing {
    Current,
    Owed,
}

const ASSERTION_ATTRIBUTE: &str = "rustc_expected_metadata_state";

pub(crate) fn witness_self_test() {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    #[cfg(unix)]
    let statuses = [0, 1 << 8, 2 << 8, 101 << 8, 9].map(ExitStatus::from_raw);
    #[cfg(windows)]
    let statuses = [0, 1, 2, 101, 0xc0000005].map(ExitStatus::from_raw);
    let [success, rejected, unexpected, panicked, terminated] = statuses;
    let rejection = serde_json::json!({
        "$message_type": "diagnostic",
        "message": "x",
        "level": "error",
        "spans": [],
        "rendered": "error: x\n",
    })
    .to_string();
    let assertion = serde_json::json!({
        "$message_type": "diagnostic",
        "message": "reworded",
        "level": "error",
        "spans": [{
            "is_primary": true,
            "text": [{ "text": "#![rustc_expected_metadata_state(cfg = \"expect_reused\")]" }],
        }],
        "rendered": "error: reworded\n",
    })
    .to_string();
    for (status, stderr, expected) in [
        (success, "", Ok(Build::Built { diagnostics: Diagnostics::default() })),
        (rejected, "", Ok(Build::Rejected { diagnostics: Diagnostics::default() })),
        (
            rejected,
            rejection.as_str(),
            Ok(Build::Rejected { diagnostics: Diagnostics(vec!["error: x\n".to_owned()]) }),
        ),
        (unexpected, "", Err(BuildFailure::Crashed { stderr: String::new() })),
        (panicked, "", Err(BuildFailure::Crashed { stderr: String::new() })),
        (terminated, "", Err(BuildFailure::Crashed { stderr: String::new() })),
        (
            rejected,
            "internal compiler error",
            Err(BuildFailure::Crashed { stderr: "internal compiler error".to_owned() }),
        ),
        (rejected, "panicked at", Err(BuildFailure::Crashed { stderr: "panicked at".to_owned() })),
        (
            rejected,
            "Found unstable fingerprints",
            Err(BuildFailure::Crashed { stderr: "Found unstable fingerprints".to_owned() }),
        ),
        (
            rejected,
            assertion.as_str(),
            Err(BuildFailure::UnexpectedIncrementalState {
                diagnostics: Diagnostics(vec!["error: reworded\n".to_owned()]),
            }),
        ),
    ] {
        let output = Output { status, stdout: Vec::new(), stderr: stderr.as_bytes().to_vec() };
        assert_eq!(Build::from_output(output.into()), expected);
    }

    let mutations = [
        (
            "a stale skip",
            Fault::SkipCrate(Crate::A),
            vec![Edit::SetAPrivateMarker(3)],
            Witness::CleanIncrementalMirEquivalence,
        ),
        (
            "an unjustified rebuild",
            Fault::RebuildEverything,
            vec![Edit::SetCPrivateMarker(3)],
            Witness::EdgeLocalScheduling,
        ),
        (
            "a stale binary",
            Fault::SkipCrate(Crate::C),
            vec![Edit::SetValue(3)],
            Witness::CleanIncrementalProbeEquivalence,
        ),
        (
            "a stale binary behind a private edit",
            Fault::SkipCrate(Crate::C),
            vec![Edit::SetAPrivateMarker(3)],
            Witness::EdgeLocalScheduling,
        ),
        (
            "a stale provider",
            Fault::SkipCrate(Crate::B),
            vec![Edit::SetValue(3)],
            Witness::CleanIncrementalAcceptanceEquivalence,
        ),
        (
            "a crate compiled twice in one transition",
            Fault::CompileTwice(Crate::A),
            vec![Edit::SetAPrivateMarker(3)],
            Witness::ExpectedMetadataState,
        ),
        (
            "a scheduler blind to upstream artifacts",
            Fault::BlindToArtifacts,
            vec![Edit::SetAPrivateMarker(3)],
            Witness::CleanIncrementalMetadataEquality,
        ),
        (
            "a stale skip while the graph is rejected",
            Fault::SkipCrate(Crate::A),
            vec![Edit::BreakBackend(Crate::C), Edit::SetAPrivateMarker(3)],
            Witness::CleanIncrementalMirEquivalence,
        ),
        (
            "a scheduler blind to session changes",
            Fault::BlindToSession,
            vec![Edit::SetThreads(Threads::Two)],
            Witness::EdgeLocalScheduling,
        ),
    ];
    let quiet = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let outcomes: Vec<_> = mutations
        .iter()
        .map(|(_, fault, edits, _)| {
            panic::catch_unwind(AssertUnwindSafe(|| {
                execute_with(Some(*fault), MirEmission::Emitted, edits)
            }))
        })
        .collect();
    panic::set_hook(quiet);

    for ((mutation, fault, edits, expected), outcome) in mutations.into_iter().zip(outcomes) {
        let Err(payload) = outcome else {
            panic!("{fault:?} passed every witness on {edits:?}");
        };
        let failure = match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(payload) => match payload.downcast::<&str>() {
                Ok(message) => message.to_string(),
                Err(_) => String::from("<non-string panic payload>"),
            },
        };
        assert!(
            failure.starts_with(&format!("{expected:?}:")),
            "{mutation} must be caught by {expected:?}, but the failure was: {failure}"
        );
    }
}

fn execute_with(fault: Option<Fault>, mir: MirEmission, forward: &[Edit]) {
    let (mut history, baseline) = History::start(fault, mir, Model::initial());
    for edit in forward {
        history.apply(edit.clone());
    }
    history.restore(baseline);
}

impl History {
    fn start(fault: Option<Fault>, mir: MirEmission, initial: Model) -> (History, Baseline) {
        let root = tempfile::tempdir_in(std::fs::canonicalize(cwd()).unwrap()).unwrap();
        let root_path = Utf8Path::from_path(root.path()).unwrap();
        let src = root_path.join("src");
        let incremental = root_path.join("incremental");
        rfs::create_dir_all(&src);
        let finalized = Crate::BUILD_ORDER
            .into_iter()
            .filter(|krate| match krate.kind() {
                CrateKind::Library => true,
                CrateKind::Binary => false,
            })
            .map(|krate| (krate, CrateInputs::of(&initial, krate)))
            .collect();
        let mut history = History {
            root,
            src,
            incremental,
            fault,
            mir,
            model: initial.clone(),
            initial,
            edits: Vec::new(),
            inverses: Vec::new(),
            last: None,
            sources: FxHashMap::default(),
            published: FxHashMap::default(),
            finalized,
        };
        history.sources = history.write_sources();
        let mut baseline_diagnostics = FxHashMap::default();
        for krate in Crate::BUILD_ORDER {
            let build = history.compile(krate, Site::IncrementalBaseline);
            let diagnostics = witness(Witness::ModelOutcomeAgreement, || match build {
                Build::Built { diagnostics } => Ok(diagnostics),
                Build::Rejected { diagnostics } => Err(format!(
                    "`{}` was rejected at the baseline, which the model always accepts \
                     (transitions: {:?}):\n{diagnostics}",
                    krate.name(),
                    history.edits
                )),
            });
            baseline_diagnostics.insert(krate, diagnostics);
            let outputs = history.outputs(&history.incremental, krate);
            let session = history.model.session;
            history
                .published
                .insert(krate, Published { outputs, session, standing: Standing::Current });
        }
        witness(Witness::ObservationRecorded, || {
            for consumer in Crate::BUILD_ORDER {
                let observed =
                    history.dep_info(&history.incremental.join(format!("{}.d", consumer.name())));
                for &provider in &Crate::BUILD_ORDER[..consumer.position()] {
                    for artifact in history.consumed_artifacts(consumer, provider) {
                        if !observed.0.contains(&artifact) {
                            return Err(format!(
                                "`{}`'s dependency information does not record {} from its \
                                 provider `{}`: {:?}",
                                consumer.name(),
                                artifact,
                                provider.name(),
                                observed.0
                            ));
                        }
                    }
                }
            }
            Ok(())
        });
        let baseline_probe = probe(&history.incremental);
        let baseline_clean = history.clean_build(CleanPoint::Baseline);
        let baseline_clean_probe =
            witness(Witness::CleanIncrementalAcceptanceEquivalence, || match &baseline_clean {
                CleanGraph::Ran { built: _, probe } => Ok(probe),
                CleanGraph::Rejected { built: _, krate, diagnostics } => Err(format!(
                    "the clean graph rejected `{}` at the baseline, which the incremental graph \
                 accepted (transitions: {:?}):\n{diagnostics}",
                    krate.name(),
                    history.edits
                )),
            });
        witness(Witness::CleanIncrementalProbeEquivalence, || {
            if baseline_probe == *baseline_clean_probe {
                return Ok(());
            }
            Err(format!(
                "incremental and clean probes disagree at the baseline: {baseline_probe:?} \
                 versus {baseline_clean_probe:?} (transitions: {:?})",
                history.edits
            ))
        });
        witness(Witness::CleanIncrementalMirEquivalence, || {
            for CleanBuild { krate, diagnostics: _, outputs } in baseline_clean.built() {
                if history.published[krate].outputs.mir != outputs.mir {
                    return Err(format!(
                        "`{}` MIR differs between the incremental and clean graphs at the baseline",
                        krate.name()
                    ));
                }
            }
            Ok(())
        });
        history.check_diagnostics(&baseline_diagnostics, &baseline_clean, "the baseline");
        history.check_model(&baseline_clean, "the baseline");
        let baseline = Baseline {
            probe: baseline_probe,
            published: history.published.clone(),
            clean: baseline_clean,
        };
        (history, baseline)
    }

    fn apply(&mut self, edit: Edit) {
        let inverse = self.model.apply(&edit);
        self.inverses.push(inverse);
        self.transition(edit);
    }

    fn restore(mut self, baseline: Baseline) {
        while let Some(inverse) = self.inverses.pop() {
            self.model.apply(&inverse);
            self.transition(inverse);
        }
        witness(Witness::MutationInversePairing, || {
            if self.model == self.initial {
                return Ok(());
            }
            Err(format!(
                "the reverse half did not restore the initial model (transitions: {:?})",
                self.edits
            ))
        });
        let restored_probe = probe(&self.incremental);
        witness(Witness::ArtifactRestoration, || {
            if restored_probe != baseline.probe {
                return Err(format!(
                    "the restored incremental graph behaves differently from the baseline \
                     (transitions: {:?})",
                    self.edits
                ));
            }
            for krate in Crate::BUILD_ORDER {
                if self.published[&krate].outputs.mir != baseline.published[&krate].outputs.mir {
                    return Err(format!(
                        "`{}` MIR in the restored incremental graph differs from the baseline \
                         (transitions: {:?})",
                        krate.name(),
                        self.edits
                    ));
                }
            }
            for krate in Crate::BUILD_ORDER {
                for (path, bytes) in &self.published[&krate].outputs.artifacts {
                    if *bytes != baseline.published[&krate].outputs.artifacts[path] {
                        return Err(format!(
                            "{} in the restored incremental graph differs from the baseline \
                             (transitions: {:?})",
                            path, self.edits
                        ));
                    }
                }
            }
            Ok(())
        });
        let restored_clean = self.clean_build(CleanPoint::Restored);
        witness(Witness::HistoryIndependentCleanArtifacts, || match &restored_clean {
            CleanGraph::Ran { built, probe: _ } => {
                let baseline = baseline.clean.built();
                if built.len() == baseline.len()
                    && built.iter().zip(baseline).all(|(restored, baseline)| {
                        restored.krate == baseline.krate
                            && restored.outputs.artifacts == baseline.outputs.artifacts
                    })
                {
                    return Ok(());
                }
                Err(format!(
                    "clean artifacts of the restored state differ from the baseline \
                     (transitions: {:?})",
                    self.edits
                ))
            }
            CleanGraph::Rejected { built: _, krate, diagnostics } => Err(format!(
                "the clean graph rejected `{}` in the restored state, which it accepted at the \
                 baseline (transitions: {:?}):\n{diagnostics}",
                krate.name(),
                self.edits
            )),
        });
    }

    fn transition(&mut self, edit: Edit) {
        let transition = self.last.map_or(Transition::FIRST, Transition::next);
        self.last = Some(transition);
        let label = format!("transition {transition} ({edit:?})");
        self.edits.push(edit);

        let outcome = self.model.outcome();
        let mut expected = FxHashMap::default();
        for krate in Crate::BUILD_ORDER {
            match krate.kind() {
                CrateKind::Library => {}
                CrateKind::Binary => continue,
            }
            let finalizes = match &outcome {
                Outcome::Runs { probe: _ } => true,
                Outcome::Rejected { krate: rejected } => krate.position() < rejected.position(),
            };
            if !finalizes {
                continue;
            }
            let current = CrateInputs::of(&self.model, krate);
            let prediction = match SessionRelation::between(&self.finalized[&krate], &current) {
                SessionRelation::Discarded => Some(MetadataState::Discarded),
                SessionRelation::Loaded(InputDelta::OwnSource) => Some(MetadataState::Changed),
                SessionRelation::Loaded(InputDelta::UpstreamOnly) => None,
                SessionRelation::Loaded(InputDelta::Unchanged) => Some(MetadataState::Reused),
            };
            if let Some(state) = prediction {
                expected.insert(krate, state);
            }
            self.finalized.insert(krate, current);
        }

        let sources = self.write_sources();
        let previous_sources = std::mem::replace(&mut self.sources, sources);
        let mut changed: FxHashSet<Utf8PathBuf> = FxHashSet::default();
        for (path, bytes) in &self.sources {
            if previous_sources.get(path) != Some(bytes) {
                changed.insert(path.clone());
            }
        }

        let before: FxHashMap<Crate, (Session, Standing)> = self
            .published
            .iter()
            .map(|(krate, published)| (*krate, (published.session, published.standing)))
            .collect();
        let mut crate_steps = Vec::new();
        let mut crates = Crate::BUILD_ORDER.into_iter();
        for krate in crates.by_ref() {
            let step = match self.schedule(krate, &changed) {
                Decision::Skip(observed) => CrateStep::Skipped { observed },
                Decision::Run(observed) => {
                    let site = Site::Incremental { expected: expected.get(&krate).copied() };
                    let mut build = self.compile(krate, site);
                    if self.fault == Some(Fault::CompileTwice(krate))
                        && let Build::Built { diagnostics: _ } = build
                    {
                        build = self.compile(krate, site);
                    }
                    match build {
                        Build::Built { diagnostics } => {
                            let current = self.outputs(&self.incremental, krate);
                            changed.extend(
                                changed_paths(
                                    &self.published[&krate].outputs.artifacts,
                                    &current.artifacts,
                                )
                                .into_iter()
                                .map(|name| self.incremental.join(name)),
                            );
                            self.published.insert(
                                krate,
                                Published {
                                    outputs: current,
                                    session: self.model.session,
                                    standing: Standing::Current,
                                },
                            );
                            CrateStep::Built { observed, diagnostics }
                        }
                        Build::Rejected { diagnostics } => {
                            self.published.get_mut(&krate).unwrap().standing = Standing::Owed;
                            CrateStep::Rejected { observed, diagnostics }
                        }
                    }
                }
            };
            crate_steps.push((krate, step));
            if let Some((_, CrateStep::Rejected { observed: _, diagnostics: _ })) =
                crate_steps.last()
            {
                break;
            }
        }
        for krate in crates {
            self.published.get_mut(&krate).unwrap().standing = Standing::Owed;
            crate_steps.push((krate, CrateStep::Blocked));
        }
        let rejected = crate_steps.iter().find_map(|(krate, step)| match step {
            CrateStep::Rejected { observed: _, diagnostics } => Some((*krate, diagnostics)),
            CrateStep::Blocked
            | CrateStep::Skipped { observed: _ }
            | CrateStep::Built { observed: _, diagnostics: _ } => None,
        });
        let diagnostics: FxHashMap<Crate, Diagnostics> = crate_steps
            .iter()
            .filter_map(|(krate, step)| match step {
                CrateStep::Built { observed: _, diagnostics }
                | CrateStep::Rejected { observed: _, diagnostics } => {
                    Some((*krate, diagnostics.clone()))
                }
                CrateStep::Blocked | CrateStep::Skipped { observed: _ } => None,
            })
            .collect();

        let clean = self.clean_build(CleanPoint::After(transition));
        witness(Witness::CleanIncrementalAcceptanceEquivalence, || match (&rejected, &clean) {
            (None, CleanGraph::Ran { built: _, probe: _ }) => Ok(()),
            (
                Some((incremental, diagnostics)),
                CleanGraph::Rejected { built: _, krate: clean, diagnostics: _ },
            ) => {
                if incremental == clean {
                    return Ok(());
                }
                Err(format!(
                    "the incremental graph rejected `{}` after {label} but the clean graph \
                     rejected `{}` (transitions: {:?}):\n{diagnostics}",
                    incremental.name(),
                    clean.name(),
                    self.edits
                ))
            }
            (Some((krate, diagnostics)), CleanGraph::Ran { built: _, probe: _ }) => Err(format!(
                "the incremental graph rejected `{}` after {label} but the clean graph \
                     accepted it (transitions: {:?}):\n{diagnostics}",
                krate.name(),
                self.edits
            )),
            (None, CleanGraph::Rejected { built: _, krate, diagnostics }) => Err(format!(
                "the incremental graph accepted {label} but the clean graph rejected `{}` \
                     (transitions: {:?}):\n{diagnostics}",
                krate.name(),
                self.edits
            )),
        });
        self.check_diagnostics(&diagnostics, &clean, &label);
        match &clean {
            CleanGraph::Ran { built: _, probe: clean_probe } => {
                let probe = probe(&self.incremental);
                witness(Witness::CleanIncrementalProbeEquivalence, || {
                    if probe == *clean_probe {
                        return Ok(());
                    }
                    Err(format!(
                        "incremental and clean probes disagree after {label}: {probe:?} \
                         versus {clean_probe:?} (transitions: {:?})",
                        self.edits
                    ))
                });
            }
            CleanGraph::Rejected { built: _, krate: _, diagnostics: _ } => {}
        }
        witness(Witness::CleanIncrementalMirEquivalence, || {
            for CleanBuild { krate, diagnostics: _, outputs } in clean.built() {
                if self.published[krate].outputs.mir != outputs.mir {
                    return Err(format!(
                        "`{}` MIR differs between the incremental and clean graphs after \
                         {label} (transitions: {:?})",
                        krate.name(),
                        self.edits
                    ));
                }
            }
            Ok(())
        });
        witness(Witness::CleanIncrementalMetadataEquality, || {
            for CleanBuild { krate, diagnostics: _, outputs } in clean.built() {
                match krate.kind() {
                    CrateKind::Library => {}
                    CrateKind::Binary => continue,
                }
                let rmeta = Utf8PathBuf::from(format!("lib{}.rmeta", krate.name()));
                if self.published[krate].outputs.artifacts[&rmeta] != outputs.artifacts[&rmeta] {
                    return Err(format!(
                        "{} differs between the incremental and clean graphs after {label} \
                         (transitions: {:?})",
                        rmeta, self.edits
                    ));
                }
            }
            Ok(())
        });
        witness(Witness::CleanIncrementalCodegenEquality, || {
            for CleanBuild { krate, diagnostics: _, outputs } in clean.built() {
                for (name, clean_bytes) in &outputs.artifacts {
                    if self.published[krate].outputs.artifacts[name] != *clean_bytes {
                        return Err(format!(
                            "{} differs between the incremental and clean graphs after \
                             {label} (transitions: {:?})",
                            name, self.edits
                        ));
                    }
                }
            }
            Ok(())
        });
        self.check_model(&clean, &label);
        witness(Witness::EdgeLocalScheduling, || {
            for (krate, step) in &crate_steps {
                let (run, observed) = match step {
                    CrateStep::Blocked => continue,
                    CrateStep::Skipped { observed } => (false, observed),
                    CrateStep::Built { observed, diagnostics: _ }
                    | CrateStep::Rejected { observed, diagnostics: _ } => (true, observed),
                };
                let source = self.src.join(format!("{}.rs", krate.name()));
                let upstream = Crate::BUILD_ORDER[..krate.position()].iter().any(|provider| {
                    self.consumed_artifacts(*krate, *provider)
                        .iter()
                        .any(|artifact| changed.contains(artifact))
                });
                let (session, standing) = before[krate];
                let required = changed.contains(&source)
                    || upstream
                    || session != self.model.session
                    || match standing {
                        Standing::Owed => true,
                        Standing::Current => false,
                    };
                match (run, required) {
                    (true, true) | (false, false) => {}
                    (true, false) => {
                        return Err(format!(
                            "`{}` was rebuilt after {label} although none of its inputs \
                             changed (transitions: {:?})",
                            krate.name(),
                            self.edits
                        ));
                    }
                    (false, true) => {
                        return Err(format!(
                            "`{}` was skipped after {label} although its source, its session, \
                             or an upstream artifact it consumes changed; its dependency \
                             information observed {observed:?} (transitions: {:?})",
                            krate.name(),
                            self.edits
                        ));
                    }
                }
            }
            Ok(())
        });
    }

    fn write_sources(&self) -> FxHashMap<Utf8PathBuf, Vec<u8>> {
        let mut sources = FxHashMap::default();
        for krate in Crate::BUILD_ORDER {
            let path = self.src.join(format!("{}.rs", krate.name()));
            let text = self.model.render(krate);
            rfs::write(&path, &text);
            sources.insert(path, text.into_bytes());
        }
        sources
    }

    fn out_dir(&self, site: Site) -> Utf8PathBuf {
        match site {
            Site::IncrementalBaseline | Site::Incremental { expected: _ } => {
                self.incremental.clone()
            }
            Site::Clean(point) => {
                Utf8Path::from_path(self.root.path()).unwrap().join(match point {
                    CleanPoint::Baseline => "clean-baseline".to_owned(),
                    CleanPoint::After(transition) => format!("clean-{transition}"),
                    CleanPoint::Restored => "clean-restored".to_owned(),
                })
            }
        }
    }

    fn compile(&self, krate: Crate, site: Site) -> Build {
        let Session {
            tracked: TrackedFlags { overflow_checks, dead_code },
            untracked: UntrackedFlags { save_temps, unstable_options, codegen_units, threads },
        } = self.model.session;
        let name = krate.name();
        let out_dir = self.out_dir(site);
        let cache = out_dir.join("cache");
        rfs::create_dir_all(&cache);
        let dep_info = out_dir.join(format!("{name}.d"));
        let mir = match self.mir {
            MirEmission::Emitted => ",mir",
            MirEmission::Omitted => "",
        };
        let mut rustc = rustc();
        rustc
            .input(self.src.join(format!("{name}.rs")))
            .crate_name(name)
            .out_dir(&out_dir)
            .library_search_path(&out_dir)
            .arg("-Zbinary-dep-depinfo")
            .arg("-Zquery-dep-graph")
            .arg("--error-format=json");
        match krate.kind() {
            CrateKind::Library => {
                rustc.crate_type("rlib");
                rustc.arg(format!("--emit=link,metadata{mir},dep-info={}", dep_info));
            }
            CrateKind::Binary => {
                rustc.crate_type("bin");
                rustc.arg(format!("--emit=link{mir},dep-info={}", dep_info));
            }
        }
        if let Some(provider) = krate.direct_provider() {
            let provider = provider.name();
            rustc.extern_(provider, out_dir.join(format!("lib{provider}.rmeta")));
            rustc.extern_(provider, out_dir.join(format!("lib{provider}.rlib")));
        }
        rustc.incremental(&cache).arg("-Zincremental-verify-ich");
        match save_temps {
            TempFiles::Deleted => {}
            TempFiles::Kept => {
                rustc.arg("-Csave-temps");
            }
        }
        match unstable_options {
            UnstableOptions::Disabled => {}
            UnstableOptions::Enabled => {
                rustc.arg("-Zunstable-options");
            }
        }
        match codegen_units {
            CodegenUnits::Default => {}
            CodegenUnits::One => {
                rustc.arg("-Ccodegen-units=1");
            }
            CodegenUnits::Four => {
                rustc.arg("-Ccodegen-units=4");
            }
        }
        match threads {
            Threads::Default => {}
            Threads::Two => {
                rustc.arg("-Zthreads=2");
            }
        }
        match overflow_checks {
            OverflowChecks::Enabled => {}
            OverflowChecks::Disabled => {
                rustc.arg("-Coverflow-checks=no");
            }
        }
        match dead_code {
            DeadCodeLint::Warn => {}
            DeadCodeLint::Allow => {
                rustc.arg("-Adead_code");
            }
        }
        match site {
            Site::Incremental { expected: Some(state) } => {
                rustc.cfg(state.cfg_name());
            }
            Site::Incremental { expected: None } | Site::IncrementalBaseline | Site::Clean(_) => {}
        }
        match Build::from_output(rustc.run_unchecked()) {
            Ok(build) => build,
            Err(BuildFailure::Crashed { stderr }) => {
                witness(Witness::IncrementalCompilation, || {
                    Err(format!(
                        "`{name}` crashed in {site:?} (transitions: {:?}):\n{stderr}",
                        self.edits
                    ))
                })
            }
            Err(BuildFailure::UnexpectedIncrementalState { diagnostics }) => {
                witness(Witness::ExpectedMetadataState, || {
                    Err(format!(
                        "`{name}` did not reach its expected incremental state in {site:?} \
                         (transitions: {:?}):\n{diagnostics}",
                        self.edits
                    ))
                })
            }
        }
    }

    fn consumed_artifacts(&self, consumer: Crate, provider: Crate) -> Vec<Utf8PathBuf> {
        let rmeta = self.incremental.join(format!("lib{}.rmeta", provider.name()));
        let rlib = self.incremental.join(format!("lib{}.rlib", provider.name()));
        match consumer.kind() {
            CrateKind::Library => vec![rmeta],
            CrateKind::Binary => vec![rmeta, rlib],
        }
    }

    fn dep_info(&self, path: &Utf8Path) -> MakeDepInfo {
        let text = rfs::read_to_string(path);
        text.parse::<MakeDepInfo>()
            .unwrap_or_else(|err| panic!("malformed dependency information `{}`: {err}", path))
    }

    fn schedule(&self, krate: Crate, changed: &FxHashSet<Utf8PathBuf>) -> Decision {
        let published = &self.published[&krate];
        let session = &self.model.session;
        let observed = match published.standing {
            Standing::Owed => Observed::Owed,
            Standing::Current => {
                let dep_info = self.dep_info(&self.incremental.join(format!("{}.d", krate.name())));
                let paths = changed
                    .iter()
                    .filter(|path| dep_info.0.contains(*path))
                    .filter(|path| match self.fault {
                        Some(Fault::BlindToArtifacts) => path.starts_with(&self.src),
                        Some(
                            Fault::RebuildEverything
                            | Fault::SkipCrate(_)
                            | Fault::CompileTwice(_)
                            | Fault::BlindToSession,
                        )
                        | None => true,
                    })
                    .cloned()
                    .collect();
                let previous = match self.fault {
                    Some(Fault::BlindToSession) => *session,
                    Some(
                        Fault::RebuildEverything
                        | Fault::SkipCrate(_)
                        | Fault::CompileTwice(_)
                        | Fault::BlindToArtifacts,
                    )
                    | None => published.session,
                };
                Observed::Changes(ObservedChanges {
                    paths,
                    session: (previous != *session).then_some((previous, *session)),
                })
            }
        };
        let justified = match &observed {
            Observed::Owed => true,
            Observed::Changes(changes) => !changes.is_empty(),
        };
        let run = match self.fault {
            Some(Fault::BlindToArtifacts | Fault::CompileTwice(_) | Fault::BlindToSession)
            | None => justified,
            Some(Fault::RebuildEverything) => true,
            Some(Fault::SkipCrate(skipped)) => skipped != krate && justified,
        };
        if run { Decision::Run(observed) } else { Decision::Skip(observed) }
    }

    fn check_diagnostics(
        &self,
        incremental: &FxHashMap<Crate, Diagnostics>,
        clean: &CleanGraph,
        label: &str,
    ) {
        witness(Witness::CleanIncrementalDiagnosticsEquality, || {
            for krate in Crate::BUILD_ORDER {
                let (Some(incremental), Some(clean)) =
                    (incremental.get(&krate), clean.diagnostics(krate))
                else {
                    continue;
                };
                if incremental != clean {
                    return Err(format!(
                        "`{}` reported different diagnostics in the incremental and clean graphs \
                         after {label} (transitions: {:?}):\n--- incremental\n{incremental}\n--- \
                         clean\n{clean}",
                        krate.name(),
                        self.edits
                    ));
                }
            }
            Ok(())
        });
    }

    fn check_model(&self, clean: &CleanGraph, label: &str) {
        let outcomes = (self.model.outcome(), clean);
        let probes = witness(Witness::ModelOutcomeAgreement, || match outcomes {
            (Outcome::Runs { probe }, CleanGraph::Ran { built: _, probe: actual }) => {
                Ok(Some((probe, actual)))
            }
            (
                Outcome::Rejected { krate: expected },
                CleanGraph::Rejected { built: _, krate: actual, diagnostics },
            ) => {
                if expected == *actual {
                    return Ok(None);
                }
                Err(format!(
                    "the model expects `{}` to be rejected after {label} but `{}` was \
                     (transitions: {:?}):\n{diagnostics}",
                    expected.name(),
                    actual.name(),
                    self.edits
                ))
            }
            (Outcome::Runs { probe: _ }, CleanGraph::Rejected { built: _, krate, diagnostics }) => {
                Err(format!(
                    "`{}` was rejected after {label} although the model expects the graph to run \
                 (transitions: {:?}):\n{diagnostics}",
                    krate.name(),
                    self.edits
                ))
            }
            (Outcome::Rejected { krate }, CleanGraph::Ran { built: _, probe: _ }) => Err(format!(
                "every crate compiled after {label} although the model expects `{}` to be \
                 rejected (transitions: {:?})",
                krate.name(),
                self.edits
            )),
        });
        let Some((expected, actual)) = probes else {
            return;
        };
        witness(Witness::ModelProbeAgreement, || match actual {
            Probe::Printed(stdout) => {
                if *stdout == expected {
                    return Ok(());
                }
                Err(format!(
                    "probe output {stdout:?} disagrees with the model's {expected:?} after \
                     {label} (transitions: {:?})",
                    self.edits
                ))
            }
            Probe::Failed { stderr } => Err(format!(
                "the probe binary failed after {label} (transitions: {:?}):\n{stderr}",
                self.edits
            )),
        });
    }

    fn clean_build(&self, point: CleanPoint) -> CleanGraph {
        let site = Site::Clean(point);
        let out_dir = self.out_dir(site);
        let mut built = Vec::new();
        for krate in Crate::BUILD_ORDER {
            match self.compile(krate, site) {
                Build::Built { diagnostics } => {
                    built.push(CleanBuild {
                        krate,
                        diagnostics,
                        outputs: self.outputs(&out_dir, krate),
                    });
                }
                Build::Rejected { diagnostics } => {
                    return CleanGraph::Rejected { built, krate, diagnostics };
                }
            }
        }
        CleanGraph::Ran { built, probe: probe(&out_dir) }
    }

    fn outputs(&self, out_dir: &Utf8Path, krate: Crate) -> BuildOutputs {
        let names = match krate.kind() {
            CrateKind::Library => {
                vec![format!("lib{}.rmeta", krate.name()), format!("lib{}.rlib", krate.name())]
            }
            CrateKind::Binary => vec![bin_name(krate.name())],
        };
        let artifacts = names
            .into_iter()
            .map(|name| {
                let bytes = rfs::read(out_dir.join(&name));
                (Utf8PathBuf::from(name), bytes)
            })
            .collect();
        let mir = match self.mir {
            MirEmission::Emitted => {
                Some(renumbered_mir(&out_dir.join(format!("{}.mir", krate.name()))))
            }
            MirEmission::Omitted => None,
        };
        BuildOutputs { artifacts, mir }
    }
}

#[derive(Clone, Copy, Debug)]
enum Site {
    IncrementalBaseline,
    Incremental { expected: Option<MetadataState> },
    Clean(CleanPoint),
}

#[derive(Clone, Copy, Debug)]
enum CleanPoint {
    Baseline,
    After(Transition),
    Restored,
}

#[derive(Debug, PartialEq, Eq)]
enum Build {
    Built { diagnostics: Diagnostics },
    Rejected { diagnostics: Diagnostics },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Diagnostics(Vec<String>);

impl std::fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for rendered in &self.0 {
            f.write_str(rendered)?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BuildFailure {
    Crashed { stderr: String },
    UnexpectedIncrementalState { diagnostics: Diagnostics },
}

impl Build {
    fn from_output(output: CompletedProcess) -> Result<Self, BuildFailure> {
        let stderr = output.stderr_utf8();
        let mut rendered = Vec::new();
        let mut assertion_failed = false;
        for line in stderr.lines() {
            let Ok(diagnostic) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if diagnostic["$message_type"] != "diagnostic" {
                continue;
            }
            if let Some(text) = diagnostic["rendered"].as_str() {
                rendered.push(text.to_owned());
            }
            let on_assertion = diagnostic["spans"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|span| span["text"].as_array().into_iter().flatten())
                .any(|line| {
                    line["text"].as_str().is_some_and(|text| text.contains(ASSERTION_ATTRIBUTE))
                });
            if diagnostic["level"] == "error" && on_assertion {
                assertion_failed = true;
            }
        }
        rendered.sort();
        let diagnostics = Diagnostics(rendered);
        if output.status().success() {
            return Ok(Self::Built { diagnostics });
        }
        let crashed = ["internal compiler error", "panicked at", "Found unstable fingerprints"]
            .iter()
            .any(|marker| stderr.contains(*marker));
        if output.status().code() != Some(1) || crashed {
            return Err(BuildFailure::Crashed { stderr });
        }
        if assertion_failed {
            return Err(BuildFailure::UnexpectedIncrementalState { diagnostics });
        }
        Ok(Self::Rejected { diagnostics })
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BuildOutputs {
    artifacts: FxHashMap<Utf8PathBuf, Vec<u8>>,
    mir: Option<String>,
}

struct CleanBuild {
    krate: Crate,
    diagnostics: Diagnostics,
    outputs: BuildOutputs,
}

enum CleanGraph {
    Ran { built: Vec<CleanBuild>, probe: Probe },
    Rejected { built: Vec<CleanBuild>, krate: Crate, diagnostics: Diagnostics },
}

impl CleanGraph {
    fn built(&self) -> &[CleanBuild] {
        match self {
            CleanGraph::Ran { built, probe: _ }
            | CleanGraph::Rejected { built, krate: _, diagnostics: _ } => built,
        }
    }

    fn diagnostics(&self, krate: Crate) -> Option<&Diagnostics> {
        if let CleanGraph::Rejected { built: _, krate: rejected, diagnostics } = self
            && *rejected == krate
        {
            return Some(diagnostics);
        }
        self.built().iter().find(|build| build.krate == krate).map(|build| &build.diagnostics)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Printed(String),
    Failed { stderr: String },
}

fn probe(out_dir: &Utf8Path) -> Probe {
    let output = cmd(out_dir.join(bin_name("c"))).run_unchecked();
    if output.status().success() {
        return Probe::Printed(output.stdout_utf8());
    }
    Probe::Failed { stderr: output.stderr_utf8() }
}

fn renumbered_mir(path: &Utf8Path) -> String {
    let mir = rfs::read_to_string(path);
    let mut seen = IndexSet::<String, FxBuildHasher>::default();
    let mir = Regex::new(r"╾─*a(lloc)?([0-9]+)(\+0x[0-9a-f]+)?(<imm>)?( \([0-9]+ ptr bytes\))?─*╼")
        .unwrap()
        .replace_all(&mir, |caps: &Captures<'_>| {
            let (index, _) = seen.insert_full(caps[2].to_owned());
            let offset = caps.get(3).map_or("", |offset| offset.as_str());
            let imm = caps.get(4).map_or("", |imm| imm.as_str());
            format!("╾ALLOC{index}{offset}{imm}╼")
        })
        .into_owned();
    Regex::new(r"\balloc([0-9]+)\b")
        .unwrap()
        .replace_all(&mir, |caps: &Captures<'_>| {
            let (index, _) = seen.insert_full(caps[1].to_owned());
            format!("ALLOC{index}")
        })
        .into_owned()
}

fn changed_paths(
    previous: &FxHashMap<Utf8PathBuf, Vec<u8>>,
    current: &FxHashMap<Utf8PathBuf, Vec<u8>>,
) -> FxHashSet<Utf8PathBuf> {
    let mut changed = FxHashSet::default();
    for (path, bytes) in current {
        if previous.get(path) != Some(bytes) {
            changed.insert(path.clone());
        }
    }
    for path in previous.keys() {
        if !current.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed
}
