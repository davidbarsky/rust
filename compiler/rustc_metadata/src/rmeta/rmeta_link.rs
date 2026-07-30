//! Non-semantic link closure carried with crate object code.

use rustc_hir::def_id::StableCrateId;
use rustc_serialize::opaque::mem_encoder::MemEncoder;
use rustc_serialize::opaque::{MAGIC_END_BYTES, MemDecoder};
use rustc_serialize::{Decodable, Decoder, Encodable, Encoder};
use rustc_session::cstore::LinkagePreference;
use rustc_span::Symbol;

pub const RMETA_LINK_FILENAME: &str = "lib.rmeta-link";
pub const RMETA_LINK_SECTION: &str = ".rmeta-link";
pub const DYLIB_RMETA_LINK_SECTION: &str = ".rmeta-l";

/// Opaque late-link metadata emitted by the compiler.
pub struct RmetaLinkData(Vec<u8>);

impl AsRef<[u8]> for RmetaLinkData {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Identifies the object-code container that carries a link closure.
pub enum RmetaLinkContents {
    Rlib { rust_object_files: Vec<String>, native_lib_filenames: Vec<Option<String>> },
    Dylib,
}

pub(crate) struct LinkCrateDep {
    pub(crate) name: Symbol,
    pub(crate) stable_crate_id: StableCrateId,
    pub(crate) kind: LinkCrateDepKind,
    pub(crate) dylib_linkage: Option<LinkagePreference>,
    pub(crate) is_private: bool,
}

/// Distinguishes unconditional dependencies from dependencies selected at final link time.
pub(crate) enum LinkCrateDepKind {
    Conditional,
    Unconditional,
}

impl<S: Encoder> Encodable<S> for LinkCrateDep {
    fn encode(&self, encoder: &mut S) {
        self.name.as_str().encode(encoder);
        self.stable_crate_id.encode(encoder);
        match self.kind {
            LinkCrateDepKind::Conditional => true.encode(encoder),
            LinkCrateDepKind::Unconditional => false.encode(encoder),
        }
        self.dylib_linkage
            .map(|linkage| match linkage {
                LinkagePreference::RequireDynamic => false,
                LinkagePreference::RequireStatic => true,
            })
            .encode(encoder);
        self.is_private.encode(encoder);
    }
}

impl<D: Decoder> Decodable<D> for LinkCrateDep {
    fn decode(decoder: &mut D) -> Self {
        Self {
            name: Symbol::intern(&String::decode(decoder)),
            stable_crate_id: StableCrateId::decode(decoder),
            kind: if bool::decode(decoder) {
                LinkCrateDepKind::Conditional
            } else {
                LinkCrateDepKind::Unconditional
            },
            dylib_linkage: Option::<bool>::decode(decoder).map(|is_static| {
                if is_static {
                    LinkagePreference::RequireStatic
                } else {
                    LinkagePreference::RequireDynamic
                }
            }),
            is_private: bool::decode(decoder),
        }
    }
}

/// Carries implementation dependencies without making them semantic metadata inputs.
pub struct RmetaLink {
    pub rust_object_files: Vec<String>,
    pub native_lib_filenames: Vec<Option<String>>,
    pub(crate) link_dependencies: Vec<LinkCrateDep>,
}

impl RmetaLink {
    pub(super) fn encode(
        contents: RmetaLinkContents,
        link_dependencies: &[LinkCrateDep],
    ) -> RmetaLinkData {
        let (rust_object_files, native_lib_filenames) = match contents {
            RmetaLinkContents::Rlib { rust_object_files, native_lib_filenames } => {
                (rust_object_files, native_lib_filenames)
            }
            RmetaLinkContents::Dylib => (Vec::new(), Vec::new()),
        };
        let mut encoder = MemEncoder::new();
        rust_object_files.encode(&mut encoder);
        native_lib_filenames.encode(&mut encoder);
        link_dependencies.encode(&mut encoder);
        let mut data = encoder.finish();
        data.extend_from_slice(MAGIC_END_BYTES);
        RmetaLinkData(data)
    }

    pub fn decode(data: &[u8]) -> RmetaLink {
        let mut decoder = MemDecoder::new(data, 0).expect("rustc emitted invalid rmeta-link data");
        let rust_object_files = Vec::<String>::decode(&mut decoder);
        let native_lib_filenames = Vec::<Option<String>>::decode(&mut decoder);
        let link_dependencies = Vec::<LinkCrateDep>::decode(&mut decoder);
        RmetaLink { rust_object_files, native_lib_filenames, link_dependencies }
    }
}
