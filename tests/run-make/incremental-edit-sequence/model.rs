use std::fmt::Write as _;
use std::num::NonZero;

use run_make_support::indexmap::IndexMap;
use run_make_support::rustc_hash::FxBuildHasher;

pub(crate) type FxIndexMap<K, V> = IndexMap<K, V, FxBuildHasher>;

const BROKEN_BACKEND_FN: &str =
    "#[inline(never)]\npub fn broken_backend() {\n    unsafe { std::arch::asm!(\"missing\") }\n}\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, hegel::DefaultGenerator)]
pub(crate) enum Crate {
    A,
    B,
    C,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrateKind {
    Library,
    Binary,
}

impl Crate {
    /// Dependency order: each crate depends on every crate before it. This is the single owner of
    /// that order; scheduling, rendering, and the rejection model all derive from it.
    pub(crate) const BUILD_ORDER: [Crate; 3] = [Crate::A, Crate::B, Crate::C];

    pub(crate) fn position(self) -> usize {
        Self::BUILD_ORDER
            .iter()
            .position(|krate| *krate == self)
            .expect("every crate appears in the build order")
    }

    /// Libraries emit metadata and an rlib; the probe binary emits neither.
    pub(crate) fn kind(self) -> CrateKind {
        match self {
            Crate::A | Crate::B => CrateKind::Library,
            Crate::C => CrateKind::Binary,
        }
    }

    pub(crate) fn direct_provider(self) -> Option<Crate> {
        self.position().checked_sub(1).map(|position| Self::BUILD_ORDER[position])
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Crate::A => "a",
            Crate::B => "b",
            Crate::C => "c",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataState {
    Reused,
    Changed,
    Discarded,
}

impl MetadataState {
    pub(crate) const ALL: [MetadataState; 3] =
        [MetadataState::Reused, MetadataState::Changed, MetadataState::Discarded];

    fn attribute_value(self) -> &'static str {
        match self {
            MetadataState::Reused => "reused",
            MetadataState::Changed => "changed",
            MetadataState::Discarded => "discarded",
        }
    }

    pub(crate) fn cfg_name(self) -> &'static str {
        match self {
            MetadataState::Reused => "expect_reused",
            MetadataState::Changed => "expect_changed",
            MetadataState::Discarded => "expect_discarded",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Transition(NonZero<usize>);

impl Transition {
    pub(crate) const FIRST: Transition = Transition(NonZero::<usize>::MIN);

    pub(crate) fn next(self) -> Transition {
        Transition(self.0.checked_add(1).expect("a history has fewer than usize::MAX transitions"))
    }
}

impl std::fmt::Display for Transition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, hegel::DefaultGenerator)]
pub(crate) enum Visibility {
    Private,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, hegel::DefaultGenerator)]
pub(crate) enum Module {
    Root,
    Inner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, hegel::DefaultGenerator)]
pub(crate) struct Item {
    pub(crate) visibility: Visibility,
    pub(crate) module: Module,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signature {
    Plain,
    Scaled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inlining {
    Default,
    Inline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScaledInstance {
    LocalToB,
    SharedFromA,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldOrder {
    FirstSecond,
    SecondFirst,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverflowChecks {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeadCodeLint {
    Warn,
    Allow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TempFiles {
    Deleted,
    Kept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnstableOptions {
    Disabled,
    Enabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodegenUnits {
    Default,
    One,
    Four,
}

hegel::pretty_print_as_debug!(CodegenUnits);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Threads {
    Default,
    Two,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) tracked: TrackedFlags,
    pub(crate) untracked: UntrackedFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackedFlags {
    pub(crate) overflow_checks: OverflowChecks,
    pub(crate) dead_code: DeadCodeLint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UntrackedFlags {
    pub(crate) save_temps: TempFiles,
    pub(crate) unstable_options: UnstableOptions,
    pub(crate) codegen_units: CodegenUnits,
    pub(crate) threads: Threads,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Runs { probe: String },
    Rejected { krate: Crate },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Model {
    pub(crate) value: u32,
    pub(crate) signature: Signature,
    pub(crate) inline: Inlining,
    pub(crate) padding: u8,
    pub(crate) a_private_marker: u32,
    pub(crate) b_private_marker: u32,
    pub(crate) c_private_marker: u32,
    pub(crate) items: FxIndexMap<u32, Item>,
    pub(crate) b_uses: FxIndexMap<u32, Module>,
    pub(crate) methods: Vec<u32>,
    pub(crate) generic_factor: u32,
    pub(crate) scaled_instance: ScaledInstance,
    pub(crate) field_order: FieldOrder,
    pub(crate) limit: u32,
    pub(crate) macro_multiplier: u32,
    pub(crate) session: Session,
    pub(crate) broken_backend: Option<Crate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    SetValue(u32),
    SetSignature(Signature),
    SetInline(Inlining),
    SetPadding(u8),
    SetAPrivateMarker(u32),
    SetBPrivateMarker(u32),
    SetCPrivateMarker(u32),
    AddItem(u32, Item),
    RemoveItem(u32),
    SetVisibility(u32, Visibility),
    MoveItem(u32, Module),
    UseItem(u32, Module),
    UnuseItem(u32),
    AddMethod(u32),
    RemoveMethod(u32),
    SwapMethods(u32, u32),
    SetSaveTemps(TempFiles),
    SetUnstableOptions(UnstableOptions),
    SetCodegenUnits(CodegenUnits),
    SetThreads(Threads),
    SetOverflowChecks(OverflowChecks),
    SetDeadCodeLint(DeadCodeLint),
    SetGenericFactor(u32),
    SetScaledInstance(ScaledInstance),
    SetFieldOrder(FieldOrder),
    SetLimit(u32),
    SetMacroMultiplier(u32),
    BreakBackend(Crate),
    RepairBackend(Crate),
    Batch(Vec<Edit>),
}

impl Model {
    pub(crate) fn initial() -> Self {
        Model {
            value: 1,
            signature: Signature::Plain,
            inline: Inlining::Default,
            padding: 0,
            a_private_marker: 0,
            b_private_marker: 0,
            c_private_marker: 0,
            items: FxIndexMap::default(),
            b_uses: FxIndexMap::default(),
            methods: Vec::new(),
            generic_factor: 2,
            scaled_instance: ScaledInstance::LocalToB,
            field_order: FieldOrder::FirstSecond,
            limit: 4,
            macro_multiplier: 3,
            session: Session {
                tracked: TrackedFlags {
                    overflow_checks: OverflowChecks::Enabled,
                    dead_code: DeadCodeLint::Warn,
                },
                untracked: UntrackedFlags {
                    save_temps: TempFiles::Deleted,
                    unstable_options: UnstableOptions::Disabled,
                    codegen_units: CodegenUnits::Default,
                    threads: Threads::Default,
                },
            },
            broken_backend: None,
        }
    }

    pub(crate) fn connected() -> Self {
        Model {
            items: FxIndexMap::from_iter([(
                9,
                Item { visibility: Visibility::Public, module: Module::Root },
            )]),
            b_uses: FxIndexMap::from_iter([(9, Module::Root)]),
            methods: vec![8, 9],
            ..Model::initial()
        }
    }

    pub(crate) fn apply(&mut self, edit: &Edit) -> Edit {
        let inverse = match edit {
            Edit::SetValue(to) => {
                assert_ne!(self.value, *to, "no-op edit {edit:?}");
                Edit::SetValue(std::mem::replace(&mut self.value, *to))
            }
            Edit::SetSignature(to) => {
                assert_ne!(self.signature, *to, "no-op edit {edit:?}");
                Edit::SetSignature(std::mem::replace(&mut self.signature, *to))
            }
            Edit::SetInline(to) => {
                assert_ne!(self.inline, *to, "no-op edit {edit:?}");
                Edit::SetInline(std::mem::replace(&mut self.inline, *to))
            }
            Edit::SetPadding(to) => {
                assert_ne!(self.padding, *to, "no-op edit {edit:?}");
                Edit::SetPadding(std::mem::replace(&mut self.padding, *to))
            }
            Edit::SetAPrivateMarker(to) => {
                assert_ne!(self.a_private_marker, *to, "no-op edit {edit:?}");
                Edit::SetAPrivateMarker(std::mem::replace(&mut self.a_private_marker, *to))
            }
            Edit::SetBPrivateMarker(to) => {
                assert_ne!(self.b_private_marker, *to, "no-op edit {edit:?}");
                Edit::SetBPrivateMarker(std::mem::replace(&mut self.b_private_marker, *to))
            }
            Edit::SetCPrivateMarker(to) => {
                assert_ne!(self.c_private_marker, *to, "no-op edit {edit:?}");
                Edit::SetCPrivateMarker(std::mem::replace(&mut self.c_private_marker, *to))
            }
            Edit::AddItem(item, value) => {
                assert!(self.items.insert(*item, *value).is_none(), "inapplicable edit {edit:?}");
                Edit::RemoveItem(*item)
            }
            Edit::RemoveItem(item) => {
                let value = self.items.shift_remove(item).unwrap_or_else(|| {
                    panic!("inapplicable edit {edit:?}: the item does not exist")
                });
                Edit::AddItem(*item, value)
            }
            Edit::SetVisibility(item, to) => {
                let current = self.items.get_mut(item).unwrap_or_else(|| {
                    panic!("inapplicable edit {edit:?}: the item does not exist")
                });
                assert_ne!(current.visibility, *to, "no-op edit {edit:?}");
                Edit::SetVisibility(*item, std::mem::replace(&mut current.visibility, *to))
            }
            Edit::MoveItem(item, to) => {
                let current = self.items.get_mut(item).unwrap_or_else(|| {
                    panic!("inapplicable edit {edit:?}: the item does not exist")
                });
                assert_ne!(current.module, *to, "no-op edit {edit:?}");
                Edit::MoveItem(*item, std::mem::replace(&mut current.module, *to))
            }
            Edit::UseItem(item, module) => {
                assert!(self.b_uses.insert(*item, *module).is_none(), "no-op edit {edit:?}");
                Edit::UnuseItem(*item)
            }
            Edit::UnuseItem(item) => {
                let module = self.b_uses.shift_remove(item).unwrap();
                Edit::UseItem(*item, module)
            }
            Edit::AddMethod(id) => {
                assert!(!self.methods.contains(id), "inapplicable edit {edit:?}: method exists");
                self.methods.push(*id);
                Edit::RemoveMethod(*id)
            }
            Edit::RemoveMethod(id) => {
                assert_eq!(
                    self.methods.last(),
                    Some(id),
                    "inapplicable edit {edit:?}: only the last method may be removed"
                );
                self.methods.pop();
                Edit::AddMethod(*id)
            }
            Edit::SwapMethods(first, second) => {
                assert_ne!(first, second, "no-op edit {edit:?}");
                let first_index = self.methods.iter().position(|method| method == first);
                let second_index = self.methods.iter().position(|method| method == second);
                let (Some(first_index), Some(second_index)) = (first_index, second_index) else {
                    panic!("inapplicable edit {edit:?}: a method is not present");
                };
                self.methods.swap(first_index, second_index);
                Edit::SwapMethods(*first, *second)
            }
            Edit::SetSaveTemps(to) => {
                let current = &mut self.session.untracked.save_temps;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetSaveTemps(std::mem::replace(current, *to))
            }
            Edit::SetUnstableOptions(to) => {
                let current = &mut self.session.untracked.unstable_options;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetUnstableOptions(std::mem::replace(current, *to))
            }
            Edit::SetCodegenUnits(to) => {
                let current = &mut self.session.untracked.codegen_units;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetCodegenUnits(std::mem::replace(current, *to))
            }
            Edit::SetThreads(to) => {
                let current = &mut self.session.untracked.threads;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetThreads(std::mem::replace(current, *to))
            }
            Edit::SetOverflowChecks(to) => {
                let current = &mut self.session.tracked.overflow_checks;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetOverflowChecks(std::mem::replace(current, *to))
            }
            Edit::SetDeadCodeLint(to) => {
                let current = &mut self.session.tracked.dead_code;
                assert_ne!(current, to, "no-op edit {edit:?}");
                Edit::SetDeadCodeLint(std::mem::replace(current, *to))
            }
            Edit::SetGenericFactor(to) => {
                assert_ne!(self.generic_factor, *to, "no-op edit {edit:?}");
                Edit::SetGenericFactor(std::mem::replace(&mut self.generic_factor, *to))
            }
            Edit::SetScaledInstance(to) => {
                assert_ne!(self.scaled_instance, *to, "no-op edit {edit:?}");
                Edit::SetScaledInstance(std::mem::replace(&mut self.scaled_instance, *to))
            }
            Edit::SetFieldOrder(to) => {
                assert_ne!(self.field_order, *to, "no-op edit {edit:?}");
                Edit::SetFieldOrder(std::mem::replace(&mut self.field_order, *to))
            }
            Edit::SetLimit(to) => {
                assert_ne!(self.limit, *to, "no-op edit {edit:?}");
                Edit::SetLimit(std::mem::replace(&mut self.limit, *to))
            }
            Edit::SetMacroMultiplier(to) => {
                assert_ne!(self.macro_multiplier, *to, "no-op edit {edit:?}");
                Edit::SetMacroMultiplier(std::mem::replace(&mut self.macro_multiplier, *to))
            }
            Edit::Batch(edits) => {
                let inverses: Vec<Edit> = edits.iter().map(|edit| self.apply(edit)).collect();
                Edit::Batch(inverses.into_iter().rev().collect())
            }
            Edit::BreakBackend(krate) => {
                assert_eq!(
                    self.broken_backend.replace(*krate),
                    None,
                    "inapplicable edit {edit:?}: a backend is already broken"
                );
                Edit::RepairBackend(*krate)
            }
            Edit::RepairBackend(krate) => {
                assert_eq!(
                    self.broken_backend.take(),
                    Some(*krate),
                    "inapplicable edit {edit:?}: that backend is not broken"
                );
                Edit::BreakBackend(*krate)
            }
        };
        self.items.sort_unstable_keys();
        self.b_uses.sort_unstable_keys();
        inverse
    }

    fn render_item_fns(&self, source: &mut String, module: Module) {
        for (item, value) in &self.items {
            if value.module != module {
                continue;
            }
            let visibility = match value.visibility {
                Visibility::Private => "",
                Visibility::Public => "pub ",
            };
            writeln!(source, "{visibility}fn item_{item}() -> u32 {{\n    {}\n}}\n", 1u32 << item)
                .unwrap();
        }
    }

    pub(crate) fn outcome(&self) -> Outcome {
        if self.broken_backend == Some(Crate::A) {
            return Outcome::Rejected { krate: Crate::A };
        }
        for (item, module) in &self.b_uses {
            match self.items.get(item) {
                Some(Item { visibility: Visibility::Public, module: actual })
                    if actual == module => {}
                Some(Item { visibility: Visibility::Public | Visibility::Private, module: _ })
                | None => return Outcome::Rejected { krate: Crate::B },
            }
        }
        if let Some(krate) = self.broken_backend {
            return Outcome::Rejected { krate };
        }
        let scale = match self.signature {
            Signature::Plain => 1,
            Signature::Scaled => 2,
        };
        let uses: u32 = self.b_uses.keys().map(|item| 1u32 << item).sum();
        let mut probe = format!(
            "value {}\nuses {uses}\nlocation {}\ngeneric {}\npair 7 9\nlimit {}\nmacro {}\n",
            self.value * scale + 1,
            u32::from(self.padding) + 2,
            5 * self.generic_factor,
            self.limit,
            3 * self.macro_multiplier
        );
        for id in &self.methods {
            writeln!(probe, "method_{id} {id}").unwrap();
        }
        Outcome::Runs { probe }
    }

    pub(crate) fn render(&self, krate: Crate) -> String {
        let mut source = String::new();
        match krate.kind() {
            CrateKind::Library => {
                source.push_str("#![feature(rustc_attrs)]\n");
                source.push_str("#![allow(internal_features)]\n");
                for state in MetadataState::ALL {
                    writeln!(
                        source,
                        "#![rustc_expected_metadata_state(cfg = \"{}\", state = \"{}\")]",
                        state.cfg_name(),
                        state.attribute_value()
                    )
                    .unwrap();
                }
            }
            CrateKind::Binary => {}
        }
        source.push('\n');
        match krate {
            Crate::A => {
                source.push_str("const ANCHOR: u32 = line!();\n");
                for _ in 0..self.padding {
                    source.push('\n');
                }
                source.push_str("pub fn location() -> u32 {\n");
                source.push_str("    std::panic::Location::caller().line() - ANCHOR\n}\n\n");
                match self.inline {
                    Inlining::Default => {}
                    Inlining::Inline => source.push_str("#[inline]\n"),
                }
                match self.signature {
                    Signature::Plain => {
                        writeln!(source, "pub fn value() -> u32 {{\n    {}\n}}\n", self.value)
                            .unwrap();
                    }
                    Signature::Scaled => {
                        writeln!(
                            source,
                            "pub fn value(scale: u32) -> u32 {{\n    {} * scale\n}}\n",
                            self.value
                        )
                        .unwrap();
                    }
                }
                writeln!(
                    source,
                    "fn private_marker() -> u32 {{\n    {}\n}}\n",
                    self.a_private_marker
                )
                .unwrap();
                self.render_item_fns(&mut source, Module::Root);
                if self.items.values().any(|item| item.module == Module::Inner) {
                    source.push_str("pub mod inner {\n");
                    self.render_item_fns(&mut source, Module::Inner);
                    source.push_str("}\n\n");
                }
                source.push_str("pub trait Shape {\n");
                for id in &self.methods {
                    writeln!(source, "    fn method_{id}(&self) -> u32;").unwrap();
                }
                source.push_str("}\n\nimpl Shape for u32 {\n");
                for id in &self.methods {
                    writeln!(source, "    fn method_{id}(&self) -> u32 {{\n        {id}\n    }}")
                        .unwrap();
                }
                source
                    .push_str("}\n\npub fn shape() -> Box<dyn Shape> {\n    Box::new(0u32)\n}\n\n");
                writeln!(
                    source,
                    "pub fn scaled<T: Into<u64>>(x: T) -> u64 {{\n    x.into() * {}\n}}\n",
                    self.generic_factor
                )
                .unwrap();
                match self.scaled_instance {
                    ScaledInstance::LocalToB => {}
                    ScaledInstance::SharedFromA => {
                        source.push_str("pub fn a_scaled() -> u64 {\n    scaled(3u32)\n}\n\n");
                    }
                }
                let fields = match self.field_order {
                    FieldOrder::FirstSecond => "    pub first: u8,\n    pub second: u64,\n",
                    FieldOrder::SecondFirst => "    pub second: u64,\n    pub first: u8,\n",
                };
                writeln!(
                    source,
                    "#[repr(C)]\n#[derive(Clone, Copy)]\npub struct Pair {{\n{fields}}}\n"
                )
                .unwrap();
                writeln!(source, "pub const LIMIT: u32 = {};\n", self.limit).unwrap();
                source.push_str("#[macro_export]\nmacro_rules! scaled_by {\n    ($e:expr) => {\n");
                writeln!(source, "        $e * {}\n    }};\n}}\n", self.macro_multiplier).unwrap();
                if self.broken_backend == Some(Crate::A) {
                    source.push_str(BROKEN_BACKEND_FN);
                }
            }
            Crate::B => {
                source.push_str(
                    "extern crate a;\n\npub use a::{LIMIT, Shape, location, scaled_by, shape};\n\n",
                );
                source.push_str("pub fn generic() -> u64 {\n    a::scaled(5u32)\n}\n\n");
                source.push_str(
                    "pub fn pair() -> a::Pair {\n    a::Pair { first: 7, second: 9 }\n}\n\n",
                );
                let call = match self.signature {
                    Signature::Plain => "a::value()",
                    Signature::Scaled => "a::value(2)",
                };
                writeln!(source, "pub fn value() -> u32 {{\n    {call} + 1\n}}\n").unwrap();
                source.push_str("pub fn uses() -> u32 {\n    0");
                for (item, module) in &self.b_uses {
                    let path = match module {
                        Module::Root => format!("a::item_{item}()"),
                        Module::Inner => format!("a::inner::item_{item}()"),
                    };
                    write!(source, " + {path}").unwrap();
                }
                source.push_str("\n}\n\n");
                writeln!(
                    source,
                    "fn private_marker() -> u32 {{\n    {}\n}}",
                    self.b_private_marker
                )
                .unwrap();
                if self.broken_backend == Some(Crate::B) {
                    source.push('\n');
                    source.push_str(BROKEN_BACKEND_FN);
                }
            }
            Crate::C => {
                source.push_str("extern crate b;\n\nmod probe {\n");
                source.push_str("    pub fn report(shape: &dyn b::Shape) {\n");
                if self.methods.is_empty() {
                    source.push_str("        let _ = shape;\n");
                }
                for id in &self.methods {
                    writeln!(
                        source,
                        "        println!(\"method_{id} {{}}\", shape.method_{id}());"
                    )
                    .unwrap();
                }
                source.push_str("    }\n}\n\nfn main() {\n");
                if self.broken_backend == Some(Crate::C) {
                    source.push_str("    unsafe { std::arch::asm!(\"missing\") }\n");
                }
                source.push_str("    let shape = b::shape();\n");
                source.push_str("    println!(\"value {}\", b::value());\n");
                source.push_str("    println!(\"uses {}\", b::uses());\n");
                source.push_str("    println!(\"location {}\", b::location());\n");
                source.push_str("    println!(\"generic {}\", b::generic());\n");
                source.push_str("    let pair = b::pair();\n");
                source.push_str("    println!(\"pair {} {}\", pair.first, pair.second);\n");
                source.push_str("    println!(\"limit {}\", b::LIMIT);\n");
                source.push_str("    println!(\"macro {}\", b::scaled_by!(3));\n");
                source.push_str("    probe::report(shape.as_ref());\n}\n\n");
                writeln!(
                    source,
                    "fn private_marker() -> u32 {{\n    {}\n}}",
                    self.c_private_marker
                )
                .unwrap();
            }
        }
        source
    }
}
