//! Standalone renderer for `__ENTRY__` (generated; do not edit).
//!
//! Pipeline: static world over the embedded file table → library built
//! from `--input key=value` pairs → translated module installed under the
//! global name `tm` (the standard host protocol) → a one-line `#include`
//! shim as the document root → `typst::compile` → export. No typst
//! toolchain, package cache, or font discovery at run time (unless
//! compiled with SYSTEM_FONTS).

use std::collections::HashMap;
use std::path::PathBuf;

use comemo::Track;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Dict, Duration, Str, Value};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::FontBook;
use typst::utils::LazyHash;
use typst::{LibraryExt, World};
use typst_layout::PagedDocument;

use __LIB_CRATE__::pkg;

mod assets;

/// Output formats baked at translate time (the runtime `--format` flag
/// overrides per invocation).
const DEFAULT_FORMATS: &[&str] = &[__FORMATS__];

/// Prefer system font discovery over the embedded typst-assets set.
const SYSTEM_FONTS: bool = __SYSTEM_FONTS__;

/// The entry file's vpath (spans and diagnostics resolve against it).
const ENTRY: &str = __ENTRY__;

fn main() {
    let mut inputs: Vec<(String, String)> = Vec::new();
    let mut formats: Vec<String> = DEFAULT_FORMATS.iter().map(|s| s.to_string()).collect();
    let mut pages: Option<String> = None;
    let mut dpi = 144.0f64;
    let mut quiet = false;
    let mut out_dir = PathBuf::from(".");
    // Root for the disk fallback of non-embedded files (typst CLI --root
    // semantics; FileId vpaths resolve under it).
    let mut root_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" => {
                let kv = args.next().unwrap_or_else(|| die("--input needs key=value"));
                let (k, v) = kv
                    .split_once('=')
                    .unwrap_or_else(|| die("--input needs key=value"));
                inputs.push((k.to_string(), v.to_string()));
            }
            "--format" => {
                formats = args
                    .next()
                    .unwrap_or_else(|| die("--format needs a list"))
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--pages" => pages = Some(args.next().unwrap_or_else(|| die("--pages needs a list"))),
            "--dpi" => {
                dpi = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| die("--dpi needs a number"))
            }
            "--root" => {
                root_dir = PathBuf::from(
                    args.next().unwrap_or_else(|| die("--root needs a directory")),
                )
            }
            "--quiet" => quiet = true,
            other => {
                if other.starts_with('-') {
                    die(&format!("unknown flag {other}"));
                }
                out_dir = PathBuf::from(other);
            }
        }
    }

    // Library carries the inputs (typst CLI semantics: inputs are part of
    // the invocation, not of the source).
    let mut dict = Dict::new();
    for (k, v) in &inputs {
        dict.insert(Str::from(k.as_str()), Value::Str(Str::from(v.as_str())));
    }
    let library = LazyHash::new(typst::Library::builder().with_inputs(dict).build());

    let mut world = StaticWorld::new(library, root_dir);

    // Install the translated module (the standard host protocol; also
    // installs the eval-closure fast path) and bind it under `tm`.
    let module = pkg::install((&world as &dyn World).track(), &world.library)
        .unwrap_or_else(|diags| {
            print_diagnostics(&world, &diags);
            std::process::exit(1);
        });
    let library = trust_runtime::bind_module(&world.library, "tm", module);
    world.set_library(library);

    let warned = typst::compile::<PagedDocument>(&world);
    if !quiet {
        for diag in &warned.warnings {
            eprintln!("warning: {}", diag.message);
        }
    }
    let document = match warned.output {
        Ok(doc) => doc,
        Err(diags) => {
            print_diagnostics(&world, &diags);
            std::process::exit(1);
        }
    };

    let selected: Vec<usize> = match &pages {
        Some(spec) => parse_pages(spec, document.pages().len()),
        None => (0..document.pages().len()).collect(),
    };
    std::fs::create_dir_all(&out_dir).unwrap_or_else(|e| die(&format!("create out dir: {e}")));
    let stem = PathBuf::from(ENTRY)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".into());
    for fmt in &formats {
        match fmt.as_str() {
            "pdf" => {
                let bytes = typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
                    .unwrap_or_else(|diags| {
                        print_diagnostics(&world, &diags);
                        std::process::exit(1);
                    });
                let path = out_dir.join(format!("{stem}.pdf"));
                std::fs::write(&path, bytes)
                    .unwrap_or_else(|e| die(&format!("write pdf: {e}")));
                println!("{}", path.display());
            }
            "svg" => {
                let opts = typst_svg::SvgOptions::default();
                for &i in &selected {
                    let svg = typst_svg::svg(&document.pages()[i], &opts);
                    let path = out_dir.join(format!("{stem}_p{:02}.svg", i + 1));
                    std::fs::write(&path, svg)
                        .unwrap_or_else(|e| die(&format!("write svg: {e}")));
                    println!("{}", path.display());
                }
            }
            "png" => {
                let opts = typst_render::RenderOptions {
                    pixel_per_pt: (dpi / 72.0).into(),
                    ..Default::default()
                };
                for &i in &selected {
                    let pixmap = typst_render::render(&document.pages()[i], &opts);
                    let path = out_dir.join(format!("{stem}_p{:02}.png", i + 1));
                    std::fs::write(
                        &path,
                        pixmap
                            .encode_png()
                            .unwrap_or_else(|e| die(&format!("encode png: {e}"))),
                    )
                    .unwrap_or_else(|e| die(&format!("write png: {e}")));
                    println!("{}", path.display());
                }
            }
            other => die(&format!("unknown format {other:?} (pdf/svg/png)")),
        }
    }
}

fn load_fonts() -> typst_kit::fonts::FontStore {
    let mut store = typst_kit::fonts::FontStore::new();
    if SYSTEM_FONTS {
        store.extend(typst_kit::fonts::system());
    }
    store.extend(typst_kit::fonts::embedded());
    store
}

/// The static world: every read is a map lookup over the embedded tables.
struct StaticWorld {
    library: LazyHash<typst::Library>,
    fonts: typst_kit::fonts::FontStore,
    book: LazyHash<FontBook>,
    main: FileId,
    sources: std::sync::Mutex<HashMap<FileId, Source>>,
    files: std::sync::Mutex<HashMap<FileId, Bytes>>,
    /// Disk root for non-embedded reads: embedded first, then disk —
    /// a pinned file (--embed) shadows the disk copy, everything else
    /// behaves like typst reading the live tree.
    disk_root: PathBuf,
}

impl StaticWorld {
    fn new(library: LazyHash<typst::Library>, disk_root: PathBuf) -> Self {
        let mut sources = HashMap::new();
        let mut files = HashMap::new();
        let intern = |vpath: &str| {
            RootedPath::new(
                VirtualRoot::Project,
                VirtualPath::new(vpath).expect("valid embedded vpath"),
            )
            .intern()
        };
        for (vpath, text) in assets::TEXTS {
            let id = intern(vpath);
            sources.insert(id, Source::new(id, text.to_string()));
            // Merged-dependency sources (`@ns/name:ver/rel`) double-mount
            // at their package root: span-driven lookups (and any
            // interpreted import) resolve package FileIds.
            if vpath.starts_with('@')
                && let Some(pid) = trust_runtime::package_root_file_id(vpath)
            {
                sources.insert(pid, Source::new(pid, text.to_string()));
            }
        }
        for (vpath, data) in assets::FILES {
            let id = intern(vpath);
            files.insert(id, Bytes::new(*data));
            // Same package-root double mount for embedded package files
            // (wasm plugins, data).
            if vpath.starts_with('@')
                && let Some(pid) = trust_runtime::package_root_file_id(vpath)
            {
                files.insert(pid, Bytes::new(*data));
            }
            // `.typ` entries double as sources (diagnostics and dynamic
            // include/import resolution read them).
            if vpath.ends_with(".typ") {
                let Ok(text) = std::str::from_utf8(data) else { continue };
                sources.insert(id, Source::new(id, text.to_string()));
                if vpath.starts_with('@')
                    && let Some(pid) = trust_runtime::package_root_file_id(vpath)
                {
                    sources.insert(pid, Source::new(pid, text.to_string()));
                }
            }
        }
        // Document root: a one-line shim including the translated module
        // (mounted at its own vpath so entry-file spans resolve against
        // the real source, not the shim).
        let main = intern("__program_main__.typ");
        sources.insert(main, Source::new(main, "#include tm\n".to_string()));
        let fonts = load_fonts();
        let book = fonts.book().clone();
        Self {
            library,
            fonts,
            book,
            main,
            sources: std::sync::Mutex::new(sources),
            files: std::sync::Mutex::new(files),
            disk_root,
        }
    }

    fn set_library(&mut self, library: LazyHash<typst::Library>) {
        self.library = library;
    }

    fn vpath(&self, id: FileId) -> String {
        id.vpath().get_with_slash().trim_start_matches('/').to_string()
    }
}

impl World for StaticWorld {
    fn library(&self) -> &LazyHash<typst::Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if let Some(source) = self.sources.lock().unwrap().get(&id) {
            return Ok(source.clone());
        }
        // Virtual closure files registered by the runtime (interpreted
        // closure/context bodies): served from the registry, memoized.
        if let Some(text) = trust_runtime::closure_source(id) {
            let source = Source::new(id, text);
            self.sources.lock().unwrap().insert(id, source.clone());
            return Ok(source);
        }
        // Disk fallback (project-root vpaths only; package files are
        // embedded-or-missing), memoized into the table.
        if id.root() == &VirtualRoot::Project
            && let Ok(text) = std::fs::read_to_string(self.disk_root.join(self.vpath(id)))
        {
            let source = Source::new(id, text);
            self.sources.lock().unwrap().insert(id, source.clone());
            return Ok(source);
        }
        Err(self.not_found(id))
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if let Some(bytes) = self.files.lock().unwrap().get(&id) {
            return Ok(bytes.clone());
        }
        // Byte view of a mounted source (the sanctioned equivalent).
        if let Some(source) = self.sources.lock().unwrap().get(&id) {
            return Ok(Bytes::from_string(source.text().to_string()));
        }
        if id.root() == &VirtualRoot::Project
            && let Ok(bytes) = std::fs::read(self.disk_root.join(self.vpath(id)))
        {
            let bytes = Bytes::new(bytes);
            self.files.lock().unwrap().insert(id, bytes.clone());
            return Ok(bytes);
        }
        Err(self.not_found(id))
    }

    fn font(&self, index: usize) -> Option<typst::text::Font> {
        self.fonts.font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        // typst CLI semantics, via the same typst-kit clock: SOURCE_DATE_EPOCH
        // pins a reproducible timestamp, otherwise the system clock; the
        // offset handling (±24h validity, date truncation) is upstream's.
        let time = match std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
        {
            Some(ts) => typst_kit::datetime::Time::fixed_timestamp(ts).ok()?,
            None => typst_kit::datetime::Time::system(),
        };
        time.today(offset)
    }
}

impl StaticWorld {
    /// Actionable miss: list the closest embedded paths and hint at
    /// `--embed` (a re-translate adds the file).
    fn not_found(&self, id: FileId) -> FileError {
        let vpath = self.vpath(id);
        let keys: Vec<String> = self
            .sources
            .lock()
            .unwrap()
            .keys()
            .chain(self.files.lock().unwrap().keys())
            .map(|k| self.vpath(*k))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let closest: Vec<&String> = keys
            .iter()
            .filter(|k| k.contains(&vpath) || vpath.contains(k.as_str()))
            .take(10)
            .collect();
        let hint = if closest.is_empty() {
            format!("embedded files: {keys:?}")
        } else {
            format!("closest embedded: {closest:?}")
        };
        eprintln!(
            "file not embedded and not under --root: {vpath} ({hint}); re-run translate with --embed or run where the file exists",
        );
        FileError::NotFound(vpath.into())
    }
}

fn parse_pages(spec: &str, total: usize) -> Vec<usize> {
    use std::num::NonZeroUsize;
    use typst::layout::{PageRange, PageRanges};

    fn bound(s: &str) -> Option<NonZeroUsize> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        NonZeroUsize::new(s.parse().unwrap_or_else(|_| die("bad --pages")))
    }

    // String → ranges stays local (upstream's parser lives in typst-cli,
    // not a library); the inclusion semantics are upstream's, open-ended
    // ranges included (`2-`, `-3`).
    let mut ranges: Vec<PageRange> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        let range = match part.split_once('-') {
            Some((a, b)) => bound(a)..=bound(b),
            None => {
                let n = bound(part).unwrap_or_else(|| die("bad --pages"));
                Some(n)..=Some(n)
            }
        };
        ranges.push(range);
    }
    let pages = PageRanges::new(ranges);
    (0..total).filter(|&i| pages.includes_page_index(i)).collect()
}

fn print_diagnostics(world: &StaticWorld, diags: &[typst::diag::SourceDiagnostic]) {
    for diag in diags {
        let loc = diag
            .span
            .id()
            .map(|id| format!(" @ {}", world.vpath(id)))
            .unwrap_or_default();
        eprintln!("error: {}{loc}", diag.message);
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
