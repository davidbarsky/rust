use std::borrow::Cow;

use rustc_error_messages::{DiagArgValue, IntoDiagArg};
use rustc_macros::{Decodable, Encodable, StableHash};

/// Different kinds of symbols can coexist even if they share the same textual name.
/// Therefore, they each have a separate universe (known as a "namespace").
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Encodable, Decodable)]
#[derive(StableHash)]
pub enum Namespace {
    /// The type namespace includes `struct`s, `enum`s, `union`s, `trait`s, and `mod`s
    /// (and, by extension, crates).
    ///
    /// Note that the type namespace includes other items; this is not an
    /// exhaustive list.
    TypeNS,
    /// The value namespace includes `fn`s, `const`s, `static`s, and local variables (including function arguments).
    ValueNS,
    /// The macro namespace includes `macro_rules!` macros, declarative `macro`s,
    /// procedural macros, attribute macros, `derive` macros, and non-macro attributes
    /// like `#[inline]` and `#[rustfmt::skip]`.
    MacroNS,
}

impl Namespace {
    /// The English description of the namespace.
    pub fn descr(self) -> &'static str {
        match self {
            Self::TypeNS => "type",
            Self::ValueNS => "value",
            Self::MacroNS => "macro",
        }
    }
}

impl IntoDiagArg for Namespace {
    fn into_diag_arg(self, _: &mut Option<std::path::PathBuf>) -> DiagArgValue {
        DiagArgValue::Str(Cow::Borrowed(self.descr()))
    }
}

/// A non-empty set of name-resolution namespaces.
///
/// The closed representation prevents item-like bindings from silently participating in no
/// namespace or an unknown namespace.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Encodable, Decodable, StableHash)]
pub enum NamespaceSet {
    Type,
    Value,
    Macro,
    TypeAndValue,
    TypeAndMacro,
    ValueAndMacro,
    All,
}

impl NamespaceSet {
    pub fn contains(self, namespace: Namespace) -> bool {
        match namespace {
            Namespace::TypeNS => match self {
                Self::Type | Self::TypeAndValue | Self::TypeAndMacro | Self::All => true,
                Self::Value | Self::Macro | Self::ValueAndMacro => false,
            },
            Namespace::ValueNS => match self {
                Self::Value | Self::TypeAndValue | Self::ValueAndMacro | Self::All => true,
                Self::Type | Self::Macro | Self::TypeAndMacro => false,
            },
            Namespace::MacroNS => match self {
                Self::Macro | Self::TypeAndMacro | Self::ValueAndMacro | Self::All => true,
                Self::Type | Self::Value | Self::TypeAndValue => false,
            },
        }
    }
}
