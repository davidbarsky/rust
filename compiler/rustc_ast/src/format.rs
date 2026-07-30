use rustc_macros::{Decodable, Encodable, Walkable};
use rustc_span::{Ident, Span, Symbol};

use crate::Expr;
use crate::token::LitKind;

// Definitions:
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
// └──────────────────────────────────────────────┘
//                     FormatArgs
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//                                     └─────────┘
//                                      argument
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//              └───────────────────┘
//                     template
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//               └────┘└─────────┘└┘
//                      pieces
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//               └────┘           └┘
//                   literal pieces
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//                     └─────────┘
//                     placeholder
//
// format_args!("hello {abc:.xyz$}!!", abc="world");
//                      └─┘  └─┘
//                      positions (could be names, numbers, empty, or `*`)

/// (Parsed) format args.
///
/// Basically the "AST" for a complete `format_args!()`.
///
/// E.g., `format_args!("hello {name}");`.
#[derive(Clone, Encodable, Decodable, Debug, Walkable)]
pub struct FormatArgs {
    pub span: Span,
    pub template: Vec<FormatArgsPiece>,
    pub arguments: FormatArguments,
    /// The raw, un-split format string literal, with no escaping or processing.
    ///
    /// Generally only useful for lints that care about the raw bytes the user wrote.
    pub uncooked_fmt_str: (LitKind, Symbol),
    /// Was the format literal written in the source?
    /// - `format!("boo")` => true,
    /// - `format!(concat!("b", "o", "o"))` => false,
    /// - `format!(include_str!("boo.txt"))` => false,
    ///
    /// If it wasn't written in the source then we have to be careful with spans pointing into it
    /// and suggestions about rewriting it.
    pub is_source_literal: bool,
}

/// A piece of a format template string.
///
/// E.g. "hello" or "{name}".
#[derive(Clone, Encodable, Decodable, Debug, Walkable)]
pub enum FormatArgsPiece {
    Literal(Symbol),
    Placeholder(FormatPlaceholder),
}

/// The arguments to format_args!().
///
/// E.g. `1, 2, name="ferris", n=3`,
/// but also implicit captured arguments like `x` in `format_args!("{x}")`.
#[derive(Clone, Encodable, Decodable, Debug, Walkable)]
pub struct FormatArguments {
    arguments: Vec<FormatArgument>,
}

impl FormatArguments {
    pub fn new() -> Self {
        Self { arguments: Vec::new() }
    }

    pub fn add(&mut self, arg: FormatArgument) -> usize {
        let index = self.arguments.len();
        if !matches!(arg.kind, FormatArgumentKind::Captured(..)) {
            assert!(
                self.arguments
                    .iter()
                    .all(|arg| !matches!(arg.kind, FormatArgumentKind::Captured(..))),
                "captured arguments must be added last"
            );
        }
        self.arguments.push(arg);
        index
    }

    pub fn by_name(&self, name: Symbol) -> Option<(usize, &FormatArgument)> {
        self.arguments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, arg)| arg.kind.ident().is_some_and(|ident| ident.name == name))
    }

    pub fn by_index(&self, i: usize) -> Option<&FormatArgument> {
        self.explicit_args().get(i)
    }

    pub fn unnamed_args(&self) -> &[FormatArgument] {
        let end = self
            .arguments
            .iter()
            .position(|arg| !matches!(arg.kind, FormatArgumentKind::Normal))
            .unwrap_or(self.arguments.len());
        &self.arguments[..end]
    }

    pub fn named_args(&self) -> &[FormatArgument] {
        let start = self
            .arguments
            .iter()
            .position(|arg| !matches!(arg.kind, FormatArgumentKind::Normal))
            .unwrap_or(self.arguments.len());
        let end = self
            .arguments
            .iter()
            .position(|arg| matches!(arg.kind, FormatArgumentKind::Captured(..)))
            .unwrap_or(self.arguments.len());
        &self.arguments[start..end]
    }

    pub fn explicit_args(&self) -> &[FormatArgument] {
        let end = self
            .arguments
            .iter()
            .position(|arg| matches!(arg.kind, FormatArgumentKind::Captured(..)))
            .unwrap_or(self.arguments.len());
        &self.arguments[..end]
    }

    pub fn all_args(&self) -> &[FormatArgument] {
        &self.arguments[..]
    }

    pub fn all_args_mut(&mut self) -> &mut Vec<FormatArgument> {
        &mut self.arguments
    }
}

#[derive(Clone, Encodable, Decodable, Debug, Walkable)]
pub struct FormatArgument {
    pub kind: FormatArgumentKind,
    pub expr: Box<Expr>,
}

#[derive(Clone, Encodable, Decodable, Debug, Walkable)]
pub enum FormatArgumentKind {
    /// `format_args(…, arg)`
    Normal,
    /// `format_args(…, arg = 1)`
    Named(Ident),
    /// `format_args("… {arg} …")`
    Captured(Ident),
}

impl FormatArgumentKind {
    pub fn ident(&self) -> Option<Ident> {
        match self {
            &Self::Normal => None,
            &Self::Named(id) => Some(id),
            &Self::Captured(id) => Some(id),
        }
    }
}

#[derive(Clone, Encodable, Decodable, Debug, PartialEq, Eq, Walkable)]
pub struct FormatPlaceholder {
    /// Index into [`FormatArgs::arguments`].
    pub argument: FormatArgPosition,
    /// The span inside the format string for the full `{…}` placeholder.
    pub span: Option<Span>,
    /// `{}`, `{:?}`, or `{:x}`, etc.
    #[visitable(ignore)]
    pub format_trait: FormatTrait,
    /// `{}` or `{:.5}` or `{:-^20}`, etc.
    #[visitable(ignore)]
    pub format_options: FormatOptions,
}

#[derive(Clone, Encodable, Decodable, Debug, PartialEq, Eq, Walkable)]
pub struct FormatArgPosition {
    /// Which argument this position refers to (Ok),
    /// or would've referred to if it existed (Err).
    #[visitable(ignore)]
    pub index: Result<usize, usize>,
    /// What kind of position this is. See [`FormatArgPositionKind`].
    #[visitable(ignore)]
    pub kind: FormatArgPositionKind,
    /// The span of the name or number.
    pub span: Option<Span>,
}

#[derive(Copy, Clone, Encodable, Decodable, Debug, PartialEq, Eq)]
pub enum FormatArgPositionKind {
    /// `{}` or `{:.*}`
    Implicit,
    /// `{1}` or `{:1$}` or `{:.1$}`
    Number,
    /// `{a}` or `{:a$}` or `{:.a$}`
    Named,
}

#[derive(Copy, Clone, Encodable, Decodable, Debug, PartialEq, Eq, Hash)]
pub enum FormatTrait {
    /// `{}`
    Display,
    /// `{:?}`
    Debug,
    /// `{:e}`
    LowerExp,
    /// `{:E}`
    UpperExp,
    /// `{:o}`
    Octal,
    /// `{:p}`
    Pointer,
    /// `{:b}`
    Binary,
    /// `{:x}`
    LowerHex,
    /// `{:X}`
    UpperHex,
}

#[derive(Clone, Encodable, Decodable, Default, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    /// The width. E.g. `{:5}` or `{:width$}`.
    pub width: Option<FormatCount>,
    /// The precision. E.g. `{:.5}` or `{:.precision$}`.
    pub precision: Option<FormatCount>,
    /// The alignment. E.g. `{:>}` or `{:<}` or `{:^}`.
    pub alignment: Option<FormatAlignment>,
    /// The fill character. E.g. the `.` in `{:.>10}`.
    pub fill: Option<char>,
    /// The `+` or `-` flag.
    pub sign: Option<FormatSign>,
    /// The `#` flag.
    pub alternate: bool,
    /// The `0` flag. E.g. the `0` in `{:02x}`.
    pub zero_pad: bool,
    /// The `x` or `X` flag (for `Debug` only). E.g. the `x` in `{:x?}`.
    pub debug_hex: Option<FormatDebugHex>,
}

#[derive(Copy, Clone, Encodable, Decodable, Debug, PartialEq, Eq)]
pub enum FormatSign {
    /// The `+` flag.
    Plus,
    /// The `-` flag.
    Minus,
}

#[derive(Copy, Clone, Encodable, Decodable, Debug, PartialEq, Eq)]
pub enum FormatDebugHex {
    /// The `x` flag in `{:x?}`.
    Lower,
    /// The `X` flag in `{:X?}`.
    Upper,
}

#[derive(Copy, Clone, Encodable, Decodable, Debug, PartialEq, Eq)]
pub enum FormatAlignment {
    /// `{:<}`
    Left,
    /// `{:>}`
    Right,
    /// `{:^}`
    Center,
}

#[derive(Clone, Encodable, Decodable, Debug, PartialEq, Eq)]
pub enum FormatCount {
    /// `{:5}` or `{:.5}`
    Literal(u16),
    /// `{:.*}`, `{:.5$}`, or `{:a$}`, etc.
    Argument(FormatArgPosition),
}
