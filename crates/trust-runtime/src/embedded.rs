//! Embedded package files: translated module crates carry their package's
//! own files (data binaries, wasm plugins, and source texts) and register
//! them here at `install()`; worlds consult the registry so a host that
//! injects the module needs no copy of the package on disk or in cache.
//!
//! Lookup order in a consulting world: its own mounts first, then this
//! registry (embedded), then disk/cache — embedded shadows nothing the
//! host explicitly mounted, and everything else reads like typst with the
//! package present.
//!
//! The registry is keyed by FileId alone: registration derives the id
//! from each entry's vkey (`@ns/name:ver/inside-package-path` for
//! package files — the FileId namespace typst's own import resolution
//! uses, since relative imports inherit the caller's root — or a plain
//! project vpath) and keeps only the bytes; the vkey itself is not
//! stored, it has no reader after the id exists.

use std::collections::HashMap;
use std::sync::OnceLock;

use typst::foundations::Bytes;
use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};

/// One embedded file: its vkey and content. The vkey follows the
/// translator's key convention — `@ns/name:ver/inside-package-path` for
/// package files, a plain project vpath otherwise. The content is bytes,
/// the single underlying channel typst itself uses (`World::file`;
/// `source()` is derived by parsing).
pub struct EmbeddedFile {
    /// The vkey (`@spec/rel` for package files, plain vpath otherwise).
    pub vkey: String,
    /// The file's content.
    pub bytes: Vec<u8>,
}

type RegistryMap = HashMap<FileId, &'static [u8]>;

static REGISTRY: OnceLock<std::sync::RwLock<RegistryMap>> = OnceLock::new();

fn registry() -> &'static std::sync::RwLock<RegistryMap> {
    REGISTRY.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

fn id_of(vkey: &str) -> Option<FileId> {
    if vkey.starts_with('@') {
        crate::package_root_file_id(vkey)
    } else {
        Some(
            RootedPath::new(VirtualRoot::Project, VirtualPath::new(vkey).ok()?).intern(),
        )
    }
}

/// Registers a set of embedded files (called by generated `install()`s;
/// the byte slices are leaked — a module's files live for the process).
pub fn register_embedded_files(files: Vec<EmbeddedFile>) {
    let mut map = registry().write().unwrap();
    for file in files {
        let EmbeddedFile { vkey, bytes } = file;
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        if let Some(id) = id_of(&vkey) {
            map.insert(id, bytes);
        }
    }
}

/// The embedded source text for a FileId, if a registered module owns
/// it — derived from the bytes (a `.typ` must be valid UTF-8; anything
/// else fails here, exactly where typst's own source loading would).
pub fn embedded_source(id: FileId) -> Option<String> {
    embedded_bytes(id).and_then(|b| String::from_utf8(b.to_vec()).ok())
}

/// The embedded bytes for a FileId, if a registered module owns it.
pub fn embedded_bytes(id: FileId) -> Option<Bytes> {
    let guard = registry().read().unwrap();
    let bytes = guard.get(&id)?;
    Some(Bytes::new(bytes.to_vec()))
}
