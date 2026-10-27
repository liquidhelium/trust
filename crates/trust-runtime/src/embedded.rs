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
//! Entries register under **both** FileId namespaces: the Project-root
//! vpath the module's internal spans and relative reads resolve through,
//! and the `@spec` package root a host's direct submodule import resolves
//! through (same content, two keys).

use std::collections::HashMap;
use std::sync::OnceLock;

use typst::foundations::Bytes;
use typst::syntax::{FileId, RootedPath, VirtualPath, VirtualRoot};

/// One embedded file: its vkey and content. The vkey follows the
/// translator's key convention — `@ns/name:ver/inside-package-path` for
/// package files (the FileId namespace typst's own import resolution uses:
/// relative imports inherit the caller's root, so a package's whole tree
/// lives under its Package root), or a plain project vpath.
pub struct EmbeddedFile {
    /// The vkey (`@spec/rel` for package files, plain vpath otherwise).
    pub vkey: String,
    /// Text form (`.typ` sources) when present.
    pub text: Option<String>,
    /// Binary form (data files, wasm) when present.
    pub data: Option<Vec<u8>>,
}

type RegistryMap = HashMap<FileId, &'static EmbeddedFile>;

static REGISTRY: OnceLock<std::sync::RwLock<RegistryMap>> = OnceLock::new();

fn registry() -> &'static std::sync::RwLock<RegistryMap> {
    REGISTRY.get_or_init(|| std::sync::RwLock::new(HashMap::new()))
}

fn project_id(vpath: &str) -> Option<FileId> {
    Some(
        RootedPath::new(VirtualRoot::Project, VirtualPath::new(vpath).ok()?).intern(),
    )
}

/// Registers a set of embedded files (called by generated `install()`s;
/// the entries are leaked — a module's files live for the process).
pub fn register_embedded_files(files: Vec<EmbeddedFile>) {
    let mut map = registry().write().unwrap();
    for file in files {
        let file: &'static EmbeddedFile = Box::leak(Box::new(file));
        for id in ids_of(file) {
            map.insert(id, file);
        }
    }
}

fn ids_of(file: &EmbeddedFile) -> Vec<FileId> {
    if file.vkey.starts_with('@') {
        crate::package_root_file_id(&file.vkey).into_iter().collect()
    } else {
        project_id(&file.vkey).into_iter().collect()
    }
}

/// The embedded source text for a FileId, if a registered module owns it.
pub fn embedded_source(id: FileId) -> Option<String> {
    registry()
        .read()
        .unwrap()
        .get(&id)
        .and_then(|f| f.text.clone())
}

/// The embedded bytes for a FileId, if a registered module owns it.
pub fn embedded_bytes(id: FileId) -> Option<Bytes> {
    registry()
        .read()
        .unwrap()
        .get(&id)
        .and_then(|f| f.data.clone())
        .map(Bytes::new)
}
