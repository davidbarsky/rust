//! Late-metadata archive member that lists which rlib entries are Rust object files,
//! and potentially other data collected and used when building or linking a rlib.
//! See <https://github.com/rust-lang/rust/issues/138243>.

use std::fs::File;
use std::path::{Path, PathBuf};

use object::read::archive::ArchiveFile;
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::memmap::Mmap;
use rustc_hir::attrs::NativeLibKind;
use rustc_metadata::{RMETA_LINK_FILENAME, RMETA_LINK_SECTION, RmetaLink};
use rustc_span::Symbol;
use rustc_target::spec::Target;
use tracing::debug;

use super::metadata::{AIX_METADATA_SYMBOL_NAME, get_metadata_xcoff, search_for_section};
use crate::NativeLib;

/// Reads the link-time metadata from an already-parsed archive.
pub fn read(archive: &ArchiveFile<'_>, archive_data: &[u8], rlib_path: &Path) -> Option<RmetaLink> {
    for entry in archive.members() {
        let entry = entry.ok()?;
        if entry.name() == RMETA_LINK_FILENAME.as_bytes() {
            let data = entry.data(archive_data).ok()?;
            let section_data = search_for_section(rlib_path, data, RMETA_LINK_SECTION).ok()?;
            return Some(RmetaLink::decode(section_data));
        }
    }
    None
}

/// Like [`read`], but parses the archive from raw bytes.
///
/// Use this when the caller's `ArchiveFile` comes from a different version of the `object` crate.
pub fn read_from_data(archive_data: &[u8], rlib_path: &Path) -> Option<RmetaLink> {
    let archive = ArchiveFile::parse(archive_data).ok()?;
    read(&archive, archive_data, rlib_path)
}

#[derive(Default)]
pub struct RmetaLinkCache {
    cache: FxHashMap<PathBuf, Option<RmetaLink>>,
}

impl RmetaLinkCache {
    pub fn get_or_insert_with(
        &mut self,
        rlib_path: &Path,
        load: impl FnOnce() -> Option<RmetaLink>,
    ) -> Option<&RmetaLink> {
        self.cache.entry(rlib_path.to_path_buf()).or_insert_with(load).as_ref()
    }

    pub fn native_lib_filenames(
        &mut self,
        target: &Target,
        rlib_path: &Path,
        native_libs: &[NativeLib],
    ) -> Vec<Option<Symbol>> {
        if !crate_may_have_bundled_libs(native_libs) {
            return Vec::new();
        }
        self.get_or_insert_with(rlib_path, || read_from_path(target, rlib_path))
            .map(|rl| {
                rl.native_lib_filenames.iter().map(|f| f.as_deref().map(Symbol::intern)).collect()
            })
            .unwrap_or_default()
    }
}

fn crate_may_have_bundled_libs(libs: &[NativeLib]) -> bool {
    libs.iter()
        .any(|lib| matches!(lib.kind, NativeLibKind::Static { bundle: Some(true) | None, .. }))
}

// FIXME: this is mostly a copy-paste of `DefaultMetadataLoader::get_rlib_metadata`.
fn read_from_path(target: &Target, path: &Path) -> Option<RmetaLink> {
    let Ok(file) = File::open(path) else {
        debug!("failed to open rlib for rmeta-link: {}", path.display());
        return None;
    };
    let Ok(mmap) = (unsafe { Mmap::map(file) }) else {
        debug!("failed to mmap rlib for rmeta-link: {}", path.display());
        return None;
    };

    if target.is_like_aix {
        let archive = ArchiveFile::parse(&*mmap).ok()?;
        for entry in archive.members() {
            let entry = entry.ok()?;
            if entry.name() == RMETA_LINK_FILENAME.as_bytes() {
                let member_data = entry.data(&*mmap).ok()?;
                let section_data =
                    get_metadata_xcoff(path, member_data, AIX_METADATA_SYMBOL_NAME).ok()?;
                return Some(RmetaLink::decode(section_data));
            }
        }
        return None;
    }

    read_from_data(&mmap, path)
}
