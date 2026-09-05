use std::str::FromStr;

use camino::Utf8PathBuf;
use indexmap::IndexSet;
use rustc_hash::FxBuildHasher;

#[cfg(test)]
mod tests;

#[derive(Debug, Eq, PartialEq)]
pub struct MakeDepInfo(pub FxIndexSet<Utf8PathBuf>);

type FxIndexSet<T> = IndexSet<T, FxBuildHasher>;

impl FromStr for MakeDepInfo {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut logical_lines = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(character) = chars.next() {
            if character != '\\' {
                logical_lines.push(character);
                continue;
            }

            let mut lookahead = chars.clone();
            let crlf_continuation =
                lookahead.next() == Some('\r') && lookahead.next() == Some('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            } else if crlf_continuation {
                chars.next();
                chars.next();
            } else {
                logical_lines.push(character);
                continue;
            }
            while chars.peek().is_some_and(|character| *character == ' ' || *character == '\t') {
                chars.next();
            }
            logical_lines.push(' ');
        }

        let mut dependencies = FxIndexSet::default();
        for line in logical_lines.lines() {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut separator = None;
            for (index, character) in line.char_indices() {
                if character != ':' {
                    continue;
                }
                let following = &line[index + character.len_utf8()..];
                if following
                    .as_bytes()
                    .first()
                    .is_none_or(|character| character.is_ascii_whitespace())
                {
                    separator = Some(index);
                    break;
                }
            }
            let Some(separator) = separator else {
                return Err(format!("Make dep-info rule has no target separator: `{line}`"));
            };

            let mut path = String::new();
            let mut characters = line[separator + 1..].chars().peekable();
            while let Some(character) = characters.next() {
                if character == '\\' && characters.peek() == Some(&' ') {
                    characters.next();
                    path.push(' ');
                } else if character.is_ascii_whitespace() {
                    if !path.is_empty() {
                        dependencies.insert(Utf8PathBuf::from(std::mem::take(&mut path)));
                    }
                } else {
                    path.push(character);
                }
            }
            if !path.is_empty() {
                dependencies.insert(Utf8PathBuf::from(path));
            }
        }

        Ok(Self(dependencies))
    }
}
