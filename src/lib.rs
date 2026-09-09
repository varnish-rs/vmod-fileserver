use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::hash::{Hash, Hasher};
#[cfg(feature = "autoindex")]
use std::io::Cursor;
use std::io::{BufRead, BufReader, Read, Take};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, SubsecRound, Utc};
use openat::Dir;
use varnish::run_vtc_tests;
use varnish::vcl::{Backend, Ctx, LogTag, StrOrBytes, VclBackend, VclResponse, VclResult};

run_vtc_tests!("tests/*.vtc");

// these exercise the actual listing generation, which is a no-op without the
// autoindex feature (a directory with no index_file just gets a 403); a
// nested module keeps their generated idents from colliding with the ones above
#[cfg(feature = "autoindex")]
mod autoindex_vtc_tests {
    ::varnish::run_vtc_tests!("tests/autoindex/*.vtc");
}

/// Serve files directly from Varnish, no external backend needed.
///
/// ```vcl
/// import fileserver;
///
/// backend default none;
///
/// sub vcl_init {
///     new www = fileserver.root("/var/www/html");
/// }
///
/// sub vcl_recv {
///     set req.backend_hint = www.backend();
/// }
/// ```
#[varnish::vmod(docs = "API.md")]
mod fileserver {
    use std::error::Error;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use openat::Dir;
    use varnish::ffi::VCL_BACKEND;
    use varnish::vcl::{Backend, Ctx};

    use super::file_backend;
    use crate::{FileBackend, build_mime_dict, validate_index_file_name};

    // Rust implementation of the VCC object, it mirrors what happens in C, except
    // for a couple of points:
    // - we create and return a Rust object, instead of a void pointer
    // - root() returns a Result, leaving the error handling to varnish-rs
    impl file_backend {
        /// Create a new file-serving backend rooted at `path`.
        ///
        /// By default, no symlink anywhere in a request's path is ever
        /// followed — see `follow_links` below.
        pub fn root(
            ctx: &mut Ctx,
            #[vcl_name] name: &str,
            /// Root directory files are served from; request URLs are
            /// resolved relative to this path.
            path: &str,
            /// Path to a `mime.types`-style file, used to populate the
            /// `content-type` header from the request URL's extension.
            ///
            /// - If absent: `/etc/mime.types` is tried, and
            ///   silently ignored if it's missing or invalid.
            /// - Empty string (`""`): disables mime-type detection entirely.
            /// - Any other path: must be a valid file, or VCL loading fails.
            ///
            /// If the file has multiple entries for the same extension, the
            /// last one wins (matching nginx/Apache).
            mime_db: Option<&str>,
            /// If `false` (default), every segment of a request's path is
            /// resolved without ever following a symlink (whether it points
            /// inside or outside `path`); a request that hits a symlink
            /// anywhere along the way fails instead of being served.
            ///
            /// If `true`, symlinks are followed unconditionally: a symlink
            /// under `path` can point anywhere on disk and will be served,
            /// which can let a request escape `path` in a low-trust,
            /// multi-tenant setup.
            #[default(false)]
            follow_links: bool,
        ) -> Result<Self, Box<dyn Error>> {
            // sanity check (note that we don't have null pointers, so path is
            // at worst empty)
            if path.is_empty() {
                return Err(format!("fileserver: can't create {name} with an empty path").into());
            }

            // store the mime database in memory, possibly
            let mimes = match mime_db {
                // if there's no path given, we try with a default one, and don't
                // complain if it fails
                None => build_mime_dict("/etc/mime.types").ok(),
                // empty strings means the user does NOT want the mime db
                Some("") => None,
                // otherwise we do want the file to be valid
                Some(p) => Some(build_mime_dict(p)?),
            };

            // unless the VCL author wants symlinks followed unconditionally,
            // open the root once now so every request can be resolved
            // relative to it, one non-symlink segment at a time
            let root_dir = if follow_links {
                None
            } else {
                Some(
                    Dir::open(path)
                        .map_err(|e| format!("fileserver: can't open root {path}: {e}"))?,
                )
            };

            let backend = Backend::new(
                ctx,
                "fileserver",
                name,
                FileBackend {
                    mimes,
                    path: path.to_string(),
                    root_dir,
                    index_files: RwLock::new(Vec::new()),
                    autoindex: AtomicBool::new(false),
                    autoindex_human_size: AtomicBool::new(true),
                    autoindex_human_dates: AtomicBool::new(true),
                },
                false,
            )?;
            Ok(file_backend { backend })
        }

        /// Add `name` to the list of filenames tried, in the order they were
        /// added, when a request resolves to a directory.
        ///
        /// `name` must be a bare filename: it can't be an absolute path, and
        /// it can't contain a `/` (no subdirectories).
        ///
        /// Can only be called from `vcl_init`.
        #[restrict(vcl_init)]
        pub fn index_file(&self, name: &str) -> Result<(), Box<dyn Error>> {
            validate_index_file_name(name)?;
            self.backend
                .get_inner()
                .index_files
                .write()
                .expect("index_files lock poisoned")
                .push(name.to_string());
            Ok(())
        }

        /// If `true`, a request that resolves to a directory with no
        /// matching `index_file` gets a generated directory listing instead
        /// of a 403. The format (HTML, JSON, or YAML) is chosen from the
        /// request's `accept` header, defaulting to HTML. A directory with
        /// more than 10,000 entries is truncated (an `x-fileserver-truncated:
        /// true` response header is added when this happens) -- the surviving
        /// entries are an arbitrary subset in directory order, not
        /// necessarily the first 10,000 alphabetically, since sorting only
        /// happens after the cap is applied.
        ///
        /// The listing is never cached (`cache-control: no-store`), since it
        /// can vary per-client (by `accept`) without bound. If unambiguous
        /// machine consumption matters, prefer the JSON format: YAML follows
        /// the YAML 1.2 Core Schema, so a filename like `no`/`yes`/`on`/`off`
        /// is emitted unquoted and a YAML *1.1* parser (e.g. Python's
        /// `yaml.safe_load`) will misread it as a boolean, not a string.
        ///
        /// Defaults to `false`. Has no effect if this build of the vmod was
        /// compiled without the `autoindex` Cargo feature (a directory with
        /// no matching `index_file` still gets a 403 either way).
        ///
        /// Can only be called from `vcl_init`.
        #[restrict(vcl_init)]
        pub fn autoindex(&self, on: bool) {
            self.backend
                .get_inner()
                .autoindex
                .store(on, Ordering::Relaxed);
        }

        /// If `true` (default), file sizes in a generated HTML directory
        /// listing are shown in a human-friendly form (e.g. `4.2K`), with
        /// the exact byte count available as a tooltip. If `false`, the
        /// exact byte count is shown directly, with no unit suffix (e.g.
        /// `4096`), matching nginx's `autoindex_exact_size` behavior.
        ///
        /// Only affects the HTML format (JSON/YAML always report the exact
        /// byte count, machine-readably). Has no effect if this build of
        /// the vmod was compiled without the `autoindex` Cargo feature.
        ///
        /// Can only be called from `vcl_init`.
        #[restrict(vcl_init)]
        pub fn autoindex_human_size(&self, on: bool) {
            self.backend
                .get_inner()
                .autoindex_human_size
                .store(on, Ordering::Relaxed);
        }

        /// If `true` (default), last-modified dates in a generated HTML
        /// directory listing get a tooltip with the precise timestamp. If
        /// `false`, the tooltip is omitted.
        ///
        /// Either way the visible date uses the same format as nginx's own
        /// autoindex (e.g. `02-Sep-2026 17:18`).
        ///
        /// Only affects the HTML format (JSON/YAML always report the exact
        /// timestamp, machine-readably). Has no effect if this build of the
        /// vmod was compiled without the `autoindex` Cargo feature.
        ///
        /// Can only be called from `vcl_init`.
        #[restrict(vcl_init)]
        pub fn autoindex_human_dates(&self, on: bool) {
            self.backend
                .get_inner()
                .autoindex_human_dates
                .store(on, Ordering::Relaxed);
        }

        /// Return the Varnish backend serving files under this object's root.
        ///
        /// - Only `GET` and `HEAD` requests are served; anything else gets a
        ///   405. A non-UTF-8 request URL gets a 400; a non-UTF-8 method
        ///   isn't `GET`/`HEAD` and so gets a 405.
        /// - The request URL's query string, if any, is ignored when
        ///   resolving the file on disk. The path itself is percent-decoded;
        ///   a malformed or unsafe percent-encoding gets a 400.
        /// - A missing file returns 404; an unreadable one returns 403; a
        ///   FIFO, socket, or device is never opened and also returns 403.
        /// - Unless `follow_links` was set on the constructor, a request
        ///   that hits a symlink anywhere in its path fails (503) instead
        ///   of being served.
        /// - A request that resolves to a directory gets a 301 (adding a
        ///   trailing slash) if it's missing one, otherwise the first
        ///   matching `index_file`, a generated listing (if `autoindex` is
        ///   on), or a 403. A trailing slash on a regular file gets a 404.
        /// - `etag`/`if-none-match` and `last-modified`/`if-modified-since`
        ///   are supported for regular files (a generated listing carries
        ///   neither, but does carry `vary: accept` and is never cached,
        ///   since it can vary per-client without bound). `etag` is derived
        ///   from the file's inode, size, and modification time (if available).
        pub unsafe fn backend(&self, _ctx: &Ctx) -> VCL_BACKEND {
            unsafe { self.backend.as_ref().vcl_ptr() }
        }
    }
}

// file_backend public functions
// it only contains backend, which wraps a FileBackend, and
// handles response body creation with a FileTransfer
#[allow(non_camel_case_types)]
struct file_backend {
    backend: Backend<FileBackend, FileTransfer>,
}

struct FileBackend {
    path: String,                           // top directory of our backend
    mimes: Option<HashMap<String, String>>, // a hashmap linking extensions to maps (optional)
    root_dir: Option<Dir>, // Some(root) unless follow_links=true; requests are resolved through it, one non-symlink segment at a time
    // candidate filenames tried (in order) when a request resolves to a
    // directory; only ever written from vcl_init (index_file() is
    // #[restrict(vcl_init)]), so request handling only ever needs a read lock
    index_files: RwLock<Vec<String>>,
    autoindex: AtomicBool,
    autoindex_human_size: AtomicBool,
    autoindex_human_dates: AtomicBool,
}

impl FileBackend {
    // looks for the first configured index_file that exists as a regular
    // file (not a directory) directly inside `dir` (the directory
    // `segments` resolves to, already opened once by the caller -- so
    // each candidate is one open relative to it, not a fresh walk from
    // root per candidate)
    fn find_index_file(
        &self,
        dir: &Dir,
        dir_path: &std::path::Path,
    ) -> Option<(File, Metadata, PathBuf)> {
        let index_files = self.index_files.read().expect("index_files lock poisoned");
        // follow_links=true still opens relative to dir's fd -- only
        // whether O_NOFOLLOW is set differs, and that's just a flag now
        let follow = self.root_dir.is_none();
        for name in index_files.iter() {
            let candidate = open_regular_at(dir, name, follow);
            let Ok(candidate_f) = candidate else {
                continue;
            };
            let Ok(candidate_meta) = candidate_f.metadata() else {
                continue;
            };
            if candidate_meta.is_dir() {
                continue;
            }
            return Some((candidate_f, candidate_meta, dir_path.join(name)));
        }
        None
    }

    // no index file matched: render a directory listing if autoindex is on,
    // otherwise 403 -- matches nginx. Compiled out entirely without the
    // `autoindex` feature; the call site falls back to the 403 directly
    // instead (see get_response)
    #[cfg(feature = "autoindex")]
    fn serve_missing_index(
        &self,
        ctx: &mut Ctx,
        dir: &Dir,
        segments: &[String],
        is_get: bool,
    ) -> VclResult<Option<FileTransfer>> {
        if self.autoindex.load(Ordering::Relaxed) {
            let bereq = ctx
                .http_bereq
                .as_ref()
                .expect("bereq is set during a backend fetch");
            let accept = bereq.header("accept").and_then(sob_helper);
            let display_path = decoded_url_path(segments);
            match self.render_autoindex(&display_path, dir, accept) {
                Ok((body, content_type, truncated)) => {
                    let beresp = ctx
                        .http_beresp
                        .as_mut()
                        .expect("beresp is set during a backend fetch");
                    beresp.set_status(200);
                    // the body depends on the accept header, but caching
                    // per distinct accept value is a foot-gun: a client
                    // can send an unbounded number of distinct accept
                    // values, and each one becomes a separate cached
                    // variant piling onto the *same* object (Varnish
                    // compares Vary-listed header values verbatim, not
                    // normalized) -- an attacker could force thousands
                    // of variants onto one URL, fragmenting cache
                    // storage and serializing lookups on that object.
                    // Generated listings are cheap enough to regenerate
                    // that it's not worth that risk: never cache them
                    beresp.set_header("vary", "accept")?;
                    beresp.set_header("cache-control", "no-store, max-age=0")?;
                    beresp.set_header("content-length", &format!("{}", body.len()))?;
                    beresp.set_header("content-type", content_type)?;
                    if truncated {
                        // MAX_LISTING_ENTRIES was hit: say so, since none of
                        // the three formats otherwise indicate the list is
                        // incomplete (a machine consumer can't tell from
                        // the body alone that entries are missing)
                        beresp.set_header("x-fileserver-truncated", "true")?;
                    }
                    let transfer = is_get.then(|| FileTransfer::Mem(Cursor::new(body)));
                    return Ok(transfer);
                }
                Err(e) => {
                    ctx.log(
                        LogTag::Error,
                        format!("fileserver: could not generate a directory listing: {e}"),
                    );
                    let beresp = ctx
                        .http_beresp
                        .as_mut()
                        .expect("beresp is set during a backend fetch");
                    beresp.set_status(403);
                    return Ok(None);
                }
            }
        }
        // index_file candidates were configured but none matched, and
        // autoindex is off: matches nginx's behavior for the same case
        let beresp = ctx
            .http_beresp
            .as_mut()
            .expect("beresp is set during a backend fetch");
        beresp.set_status(403);
        Ok(None)
    }
}

// silly helper until varnish-rs provides something more ergonomic. Returns
// None (rather than panicking) for non-UTF-8 input: a header value is
// attacker-controlled, and a panic unwinding across the extern "C" backend
// boundary aborts the whole worker process, not just this request
#[expect(clippy::needless_pass_by_value)]
fn sob_helper(sob: StrOrBytes<'_>) -> Option<&str> {
    match sob {
        StrOrBytes::Bytes(_) => None,
        StrOrBytes::Utf8(s) => Some(s),
    }
}

impl VclBackend<FileTransfer> for FileBackend {
    #[expect(clippy::too_many_lines)]
    fn get_response(&self, ctx: &mut Ctx) -> VclResult<Option<FileTransfer>> {
        // we know that bereq and bereq_url are set, so we can just expect the options
        let bereq = ctx
            .http_bereq
            .as_ref()
            .expect("bereq is set during a backend fetch");

        // let's start building our response
        let beresp = ctx
            .http_beresp
            .as_mut()
            .expect("beresp is set during a backend fetch");

        // a non-UTF-8 request URL can't be resolved to a filesystem path
        let Some(bereq_url) = sob_helper(bereq.url().expect("bereq always has a url")) else {
            beresp.set_status(400);
            return Ok(None);
        };
        // same idea: a non-UTF-8 method obviously isn't GET/HEAD either
        let method = bereq.method().and_then(sob_helper);

        // reject unsupported methods before touching the filesystem
        if method != Some("HEAD") && method != Some("GET") {
            // we are fairly strict in what method we accept
            beresp.set_status(405);
            return Ok(None);
        }
        let is_get = method == Some("GET");

        // combine root and url into something that's hopefully safe. The query
        // string (if any) is not part of the filesystem path -- nginx and Apache
        // both split path from query at the HTTP-parsing layer and never consult
        // the query string for static-file lookups, so a request like
        // "/app.js?v=123" (a common cache-busting pattern) must still resolve to
        // "/app.js" on disk, not literally fail to find a file named "app.js?v=123".
        // Segments are percent-decoded and ".."-clamped by clamp_segments(); a
        // malformed or unsafe percent-encoding (see percent_decode_segment) is
        // rejected with 400 rather than being passed through to the filesystem.
        let Ok(segments) = clamp_segments(strip_query(bereq_url)) else {
            beresp.set_status(400);
            return Ok(None);
        };
        let mut path = join_segments(&self.path, &segments);
        ctx.log(
            LogTag::Debug,
            format!("fileserver: file on disk: {}", path.display()),
        );

        // reset the bereq lifetime, otherwise we couldn't use ctx in the line above
        // yes, it feels weird at first, but it's for our own good
        let bereq = ctx
            .http_bereq
            .as_ref()
            .expect("bereq is set during a backend fetch");
        let bereq_url = sob_helper(bereq.url().expect("bereq always has a url"))
            .expect("already validated as UTF-8 above");
        let beresp = ctx
            .http_beresp
            .as_mut()
            .expect("beresp is set during a backend fetch");
        let url_path = strip_query(bereq_url);

        // open the file and get some metadata. Unless the VCL author wants
        // symlinks followed unconditionally (follow_links), walk the
        // request one non-symlink segment at a time relative to the root
        // dir we opened in root() -- a symlink anywhere along the way
        // (inside or outside the root, we don't distinguish) makes the
        // corresponding open_file/sub_dir call fail instead of following it
        let f = match &self.root_dir {
            Some(root_dir) => {
                walk_segments(root_dir, &segments, |d, s| open_regular_at(d, s, false))
            }
            None => open_regular(&path),
        };
        let mut f = match f {
            Ok(f) => f,
            // NotADirectory: a path segment expected to be a directory
            // (e.g. the "foo.txt" in "/foo.txt/bar") turned out to be a
            // regular file -- there's nothing there either way, so treat
            // it the same as NotFound rather than a hard backend error
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                beresp.set_status(404);
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                beresp.set_status(403);
                return Ok(None);
            }
            Err(e) => return Err(e.to_string().into()),
        };

        let mut metadata: Metadata = f.metadata().map_err(|e| e.to_string())?;

        if metadata.is_dir() {
            // directories need a trailing slash so relative links in a
            // generated listing (or in the index file it serves) resolve
            // correctly -- redirect if it's missing, same as nginx. The
            // Location is rebuilt from the normalized, re-escaped segments
            // (not echoed back from the raw request) so a directory that
            // happens to be named e.g. "evil.example.com" can't turn a
            // request for "//evil.example.com" into a scheme-relative
            // Location that browsers resolve as "http://evil.example.com/"
            if !url_path.ends_with('/') {
                let mut location = normalized_url_path(&segments);
                if let Some((_, q)) = bereq_url.split_once('?') {
                    location.push('?');
                    location.push_str(q);
                }
                beresp.set_status(301);
                beresp.set_header("location", &location)?;
                return Ok(None);
            }

            // the common case: nothing configured means nothing to look up
            // -- skip opening the directory at all rather than walking to
            // it just to immediately discover there's no index_file to try
            // and no listing to render
            if self
                .index_files
                .read()
                .expect("index_files lock poisoned")
                .is_empty()
                && !self.autoindex.load(Ordering::Relaxed)
            {
                beresp.set_status(403);
                return Ok(None);
            }

            // dir gives find_index_file/render_autoindex a single handle to
            // search candidates and list entries against (one hop per name,
            // not a walk from root each time). It's obtained by reusing f's
            // fd rather than reopening by path: f already points at this
            // exact directory (opening a directory O_RDONLY succeeds), and
            // a fresh reopen could resolve to something other than what
            // metadata.is_dir() just checked, if the path changed
            // underneath us in between. This works the same whether f came
            // from the NOFOLLOW walk or the follow_links=true open: naming
            // and listing entries never involves a follow/no-follow
            // decision, only the per-entry lookups downstream do
            //
            // from_raw_fd_checked does its own fstat, which can technically
            // fail, but f's type can't change after open and metadata.is_dir()
            // above already confirmed this exact fd is a directory. A failure
            // here means our own reasoning about fd types is wrong, not
            // attacker-controlled input, so it's worth surfacing loudly
            // (worker abort) rather than masking it as a client-facing 403
            let dir = unsafe { Dir::from_raw_fd_checked(f.into_raw_fd()) }
                .expect("f was already confirmed to be a directory fd by metadata.is_dir() above");

            match self.find_index_file(&dir, &path) {
                Some((idx_f, idx_meta, idx_path)) => {
                    // an index file was found: fall through to the normal
                    // file-serving logic below, as if it had been requested
                    // directly
                    f = idx_f;
                    metadata = idx_meta;
                    path = idx_path;
                }
                #[cfg(feature = "autoindex")]
                None => {
                    return self.serve_missing_index(ctx, &dir, &segments, is_get);
                }
                // no renderer compiled in: same outcome serve_missing_index
                // would reach anyway, without needing the function at all
                #[cfg(not(feature = "autoindex"))]
                None => {
                    beresp.set_status(403);
                    return Ok(None);
                }
            }
        } else if url_path.ends_with('/') {
            // a trailing slash only makes sense for a directory; matches nginx
            beresp.set_status(404);
            return Ok(None);
        }

        let cl = metadata.len();
        let modified_raw = match metadata.modified() {
            Ok(t) => Some(t),
            Err(e) => {
                ctx.log(
                    LogTag::Error,
                    format!(
                        "fileserver: could not read mtime for {}: {e}",
                        path.display()
                    ),
                );
                None
            }
        };
        // the ctx.log() call above needs exclusive access to ctx, so bereq/beresp
        // (borrowed from ctx fields) have to be re-fetched afterwards
        let bereq = ctx
            .http_bereq
            .as_ref()
            .expect("bereq is set during a backend fetch");
        let beresp = ctx
            .http_beresp
            .as_mut()
            .expect("beresp is set during a backend fetch");
        let modified: Option<DateTime<Utc>> = modified_raw.map(DateTime::from);
        let etag = generate_etag(&metadata, modified_raw);

        // can we avoid sending a body?
        let is_304 = is_not_modified(
            &etag,
            modified,
            bereq.header("if-none-match").and_then(sob_helper),
            bereq.header("if-modified-since").and_then(sob_helper),
        );

        beresp.set_proto("HTTP/1.1")?;
        let mut transfer = None;
        if is_304 {
            // 304 will save us some bandwidth
            beresp.set_status(304);
        } else {
            // "normal" request, if it's a HEAD to save a bunch of work, but if
            // it's a GET we need to add the VFP to the pipeline
            // and add a BackendResp to the priv1 field
            beresp.set_status(200);
            if is_get {
                // prevent reading more than expected
                transfer = Some(FileTransfer::File(BufReader::new(f).take(cl)));
            }
        }

        // set all the headers we can, including the content-type if we can
        beresp.set_header("content-length", &format!("{cl}"))?;
        beresp.set_header("etag", &etag)?;
        if let Some(modified) = modified {
            beresp.set_header(
                "last-modified",
                &modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
            )?;
        }

        // we only care about content-type if there's content
        if cl > 0 {
            // we need both and extension and a mime database
            if let (Some(ext), Some(h)) = (path.extension(), self.mimes.as_ref())
                && let Some(ct) = h.get(ext.to_string_lossy().as_ref())
            {
                beresp.set_header("content-type", ct)?;
            }
        }
        Ok(transfer)
    }
}

enum FileTransfer {
    File(Take<BufReader<File>>),
    #[cfg(feature = "autoindex")]
    Mem(Cursor<Vec<u8>>),
}

impl VclResponse for FileTransfer {
    fn read(&mut self, buf: &mut [u8]) -> VclResult<usize> {
        match self {
            FileTransfer::File(r) => r.read(buf).map_err(|e| e.to_string().into()),
            #[cfg(feature = "autoindex")]
            FileTransfer::Mem(r) => r.read(buf).map_err(|e| e.to_string().into()),
        }
    }
    fn len(&self) -> Option<usize> {
        match self {
            FileTransfer::File(r) => {
                Some(usize::try_from(r.limit()).expect("casting u64 to usize"))
            }
            #[cfg(feature = "autoindex")]
            FileTransfer::Mem(r) => Some(
                r.get_ref().len() - usize::try_from(r.position()).expect("casting u64 to usize"),
            ),
        }
    }
}

// hard cap on how many raw directory entries a single generated listing
// examines (not how many survive the dotfile/non-UTF-8/symlink filters --
// counting only survivors would let a directory dominated by filtered-out
// names force a full, uncapped directory scan on every request)
#[cfg(feature = "autoindex")]
const MAX_LISTING_ENTRIES: usize = 10_000;

#[cfg(feature = "autoindex")]
#[derive(serde::Serialize)]
struct AutoindexEntry {
    name: String,
    is_dir: bool,
    // None for directories: a directory's on-disk "size" is an
    // implementation detail (block usage for its own directory entries),
    // not something a listing should show, in either JSON/YAML or HTML
    size: Option<u64>,
    modified: Option<DateTime<Utc>>,
}

#[cfg(feature = "autoindex")]
enum AutoindexFormat {
    Html,
    Json,
    Yaml,
}

#[cfg(feature = "autoindex")]
impl AutoindexFormat {
    fn content_type(&self) -> &'static str {
        match self {
            AutoindexFormat::Html => "text/html; charset=utf-8",
            AutoindexFormat::Json => "application/json",
            AutoindexFormat::Yaml => "application/x-yaml",
        }
    }
}

// picks the response format from the `accept` header; a simple substring
// match, not full quality-value (q=) negotiation -- unmatched or missing
// `accept` falls back to HTML
#[cfg(feature = "autoindex")]
fn pick_autoindex_format(accept: Option<&str>) -> AutoindexFormat {
    let Some(accept) = accept else {
        return AutoindexFormat::Html;
    };
    if accept.contains("application/json") {
        AutoindexFormat::Json
    } else if accept.contains("application/x-yaml")
        || accept.contains("application/yaml")
        || accept.contains("text/yaml")
    {
        AutoindexFormat::Yaml
    } else {
        AutoindexFormat::Html
    }
}

#[cfg(feature = "autoindex")]
impl FileBackend {
    // builds a directory listing body plus its content-type, in the format
    // picked from `accept`
    fn render_autoindex(
        &self,
        url_path: &str,
        dir: &Dir,
        accept: Option<&str>,
    ) -> Result<(Vec<u8>, &'static str, bool), Box<dyn Error>> {
        let mut entries = Vec::new();
        let mut truncated = false;
        // counts every raw entry examined, not just ones that survive the
        // dotfile/non-UTF-8/symlink filters below -- capping only survivors
        // would let a directory dominated by filtered-out names force a
        // full, uncapped scan (unbounded CPU/syscalls) on every request
        let mut seen: usize = 0;
        // NOT dir.list_self(): `dir` is opened with O_PATH (like every
        // Dir in the openat crate on Linux), and fdopendir() rejects
        // O_PATH fds; list_dir(".") reopens a proper listable fd first.
        // Naming entries never involves a follow/no-follow decision, so
        // this part doesn't need to branch on follow_links
        let follow = self.root_dir.is_none();
        for entry in dir.list_dir(".")? {
            seen += 1;
            if seen > MAX_LISTING_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry?;
            // a non-UTF-8 name can't round-trip through this vmod's
            // percent-decoded path scheme, so listing it would only
            // ever produce a permanently broken href -- skip it
            // rather than advertise a dead link
            let Some(name) = entry.file_name().to_str() else {
                continue;
            };
            let name = name.to_string();
            // nginx-style: don't advertise dotfiles (.git, .htpasswd, ...)
            if name.starts_with('.') {
                continue;
            }
            let Ok(st) = fstatat(dir, entry.file_name(), follow) else {
                continue;
            };
            // !follow reports the symlink's own metadata rather than
            // following it, so skip it here, matching follow_links=false;
            // follow=true already resolved past any symlink in fstatat
            // itself, so this never trips in that mode
            if !follow && st.st_mode & libc::S_IFMT == libc::S_IFLNK {
                continue;
            }
            let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
            entries.push(AutoindexEntry {
                name,
                is_dir,
                size: (!is_dir).then_some(st.st_size.cast_unsigned()),
                modified: stat_modified(&st).map(DateTime::from),
            });
        }
        // nginx-style: directories before files, each group alphabetical
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

        let format = pick_autoindex_format(accept);
        // NOTE: unlike the HTML format (see html_escape/is_display_spoofing_char),
        // `name` is NOT sanitized for JSON/YAML: both formats' string
        // escaping is spec-compliant (JSON only mandates escaping 0x00-0x1F,
        // `"`, and `\`; DEL and C1 controls, and Unicode bidi/invisible
        // characters, are all valid unescaped JSON/YAML string content), so
        // a properly-parsing consumer is unaffected either way. The risk is
        // the same class as the YAML "Norway problem" noted below: someone
        // piping the raw, unparsed response straight into a terminal. That's
        // a much weaker threat model for a machine-consumption format than
        // for HTML (which is directly rendered), so this vmod reports the
        // real, unmodified name here rather than silently lossy-sanitizing
        // data a machine consumer may need byte-exact
        let body = match format {
            AutoindexFormat::Json => serde_json::to_vec(&entries)?,
            // NOTE: yaml_serde (like its ancestor serde_yaml) follows the
            // YAML 1.2 Core Schema, so it only quotes `name` when *that*
            // schema would otherwise misread it -- a filename like "no",
            // "yes", "on", or "off" is emitted as a bare, unquoted scalar,
            // which a YAML *1.1* parser (e.g. Python's `yaml.safe_load`,
            // the "Norway problem") reads back as a bool, not a string.
            // This is a known, longstanding disagreement between YAML
            // spec versions in the wider ecosystem, not something this
            // vmod can fix by itself without fragile post-processing of
            // the emitted text -- consumers that need an unambiguous
            // `name` field should request the JSON format instead, which
            // always quotes strings
            AutoindexFormat::Yaml => yaml_serde::to_string(&entries)?.into_bytes(),
            AutoindexFormat::Html => render_autoindex_html(
                url_path,
                &entries,
                self.autoindex_human_size.load(Ordering::Relaxed),
                self.autoindex_human_dates.load(Ordering::Relaxed),
            )
            .into_bytes(),
        };
        Ok((body, format.content_type(), truncated))
    }
}

// mimics the classic nginx/Apache `<pre>`-based autoindex layout: a page
// titled "Index of <path>", a link back to the parent directory, then one
// line per entry with the name, date, and size lined up in columns
#[cfg(feature = "autoindex")]
fn render_autoindex_html(
    url_path: &str,
    entries: &[AutoindexEntry],
    human_size: bool,
    human_dates: bool,
) -> String {
    const NAME_COL: usize = 50;
    const SIZE_COL: usize = 19;

    let title = html_escape(url_path);
    let mut out = format!(
        "<html>\n<head><title>Index of {title}</title></head>\n<body>\n<h1>Index of {title}</h1><hr><pre>"
    );
    if url_path != "/" {
        out.push_str("<a href=\"../\">../</a>\n");
    }
    for e in entries {
        let display_name = if e.is_dir {
            format!("{}/", e.name)
        } else {
            e.name.clone()
        };
        let href = url_escape(&display_name);

        // long names get truncated for column alignment, same as nginx; the
        // href above still carries the full, untruncated name. Either way at
        // least one space always separates the name from the date column:
        // a name that's exactly NAME_COL chars long must not collide with it
        let name_len = display_name.chars().count();
        let (shown_name, pad_len) = if name_len >= NAME_COL {
            let truncated: String = display_name.chars().take(NAME_COL - 3).collect();
            (format!("{truncated}..>"), 0)
        } else {
            (display_name, NAME_COL - name_len)
        };
        let name = html_escape(&shown_name);
        let name_pad = " ".repeat(pad_len + 1);

        let date_html = match e.modified {
            None => "-".to_string(),
            // nginx's own autoindex always renders this exact format; we
            // only add the tooltip (with the precise timestamp) on top when
            // human_dates is on
            Some(m) if human_dates => format!(
                "<span title=\"{}\">{}</span>",
                m.to_rfc3339(),
                m.format("%d-%b-%Y %H:%M")
            ),
            Some(m) => m.format("%d-%b-%Y %H:%M").to_string(),
        };

        let size_text = match e.size {
            None => "-".to_string(),
            Some(sz) if human_size => human_readable_size(sz),
            Some(sz) => sz.to_string(),
        };
        let size_pad = " ".repeat(SIZE_COL.saturating_sub(size_text.chars().count()));
        let size_html = match e.size {
            Some(sz) if human_size => {
                format!("<span title=\"{sz} bytes\">{size_text}</span>")
            }
            _ => size_text,
        };

        let _ = writeln!(
            out,
            "<a href=\"{href}\">{name}</a>{name_pad}{date_html}  {size_pad}{size_html}"
        );
    }
    out.push_str("</pre><hr></body>\n</html>\n");
    out
}

// escapes HTML-significant characters, and also neutralizes control
// characters (ESC, other C0/C1 controls, DEL): a filename can legally
// contain any byte except '/' and NUL, so without this, a name containing
// e.g. an ANSI escape sequence would be echoed raw into the listing text --
// harmless in a browser, but anyone viewing the raw response through a
// terminal (`curl | less`, a log pipeline, etc.) gets arbitrary terminal
// escape sequences executed (screen clears, hidden/spoofed text, and worse
// in vulnerable terminal emulators). `char::is_control()` operates on
// decoded Unicode scalar values, so this can't be confused by multi-byte
// UTF-8 sequences the way a raw-byte filter could be
#[cfg(feature = "autoindex")]
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() || is_display_spoofing_char(c) => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

// Unicode bidi-override and invisible/zero-width characters: `char::is_control()`
// only covers the C0/C1/DEL control ranges, not these ("Cf", format) -- a
// filename containing e.g. U+202E (RIGHT-TO-LEFT OVERRIDE) can make a
// browser *display* "gpj.exe" as "exe.jpg" (the classic filename-spoofing
// trick), and this works in any renderer, not just a terminal. Note this
// only sanitizes the *visible* text (via html_escape): the href is built
// separately from the real, unmodified name (see url_escape), so the link
// still resolves to the actual file regardless
#[cfg(feature = "autoindex")]
// this list is a best-effort curated set, not a formal Unicode security
// profile (see UTS #39 for that) -- it targets the characters commonly
// used in real-world filename-spoofing/homograph attacks, not every
// codepoint that is merely invisible or a formatting control
fn is_display_spoofing_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' // soft hyphen
        | '\u{061C}' // Arabic letter mark (the third implicit bidi mark, alongside LRM/RLM)
        | '\u{180E}' // Mongolian vowel separator
        | '\u{200B}'..='\u{200F}' // zero-width space/joiner/non-joiner, LRM/RLM
        | '\u{2028}'..='\u{2029}' // line/paragraph separator: can force a line
                                  // break even inside <pre>, fracturing the
                                  // column-aligned listing layout
        | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
        | '\u{2060}'..='\u{2069}' // word joiner, invisible operators, isolates
        | '\u{FE00}'..='\u{FE0F}' // variation selectors
        | '\u{FEFF}' // BOM / zero-width no-break space
        | '\u{E0001}' // language tag
        | '\u{E0020}'..='\u{E007F}' // Unicode "tag" block: invisible when
                                    // rendered, used for "ASCII smuggling"
                                    // (hiding a second, invisible payload
                                    // inside otherwise-plain-looking text)
    )
}

#[cfg(feature = "autoindex")]
fn human_readable_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

// openat::Metadata (fstatat-based) has no portable `.modified()` like
// std::fs::Metadata -- build one from the raw stat's mtime fields instead.
// st_mtime/st_mtime_nsec have the same names on Linux and macOS (the libc
// crate standardizes them), so this is portable across this vmod's
// supported platforms
#[cfg(feature = "autoindex")]
fn stat_modified(stat: &libc::stat) -> Option<SystemTime> {
    let secs = u64::try_from(stat.st_mtime).ok()?;
    let nsec = u32::try_from(stat.st_mtime_nsec).ok()?;
    if nsec >= 1_000_000_000 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, nsec))
}

// fstatat relative to dir's fd, toggling AT_SYMLINK_NOFOLLOW via `follow`
// -- same idea as open_regular_at()'s O_NOFOLLOW: follow/no-follow is
// just a flag on the syscall, not a different code path. This *stats*,
// it never opens the entry, so a FIFO with no writer can't hang the
// request, and an unreadable-but-listable file doesn't vanish from the
// listing
#[cfg(feature = "autoindex")]
fn fstatat(dir: &Dir, name: &std::ffi::OsStr, follow: bool) -> std::io::Result<libc::stat> {
    let cname = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(name))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::fstatat(dir.as_raw_fd(), cname.as_ptr(), &raw mut stat, flags) };
    if res < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat)
}

// index_file() candidates must be a bare filename: no subdirectories, no
// absolute paths, nothing that could turn a lookup into a path traversal
fn validate_index_file_name(name: &str) -> Result<(), Box<dyn Error>> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(
            format!("fileserver: index_file {name:?} must be a bare filename, not a path").into(),
        );
    }
    Ok(())
}

// reads a mime database into a hashmap, if we can
fn build_mime_dict(path: &str) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let mut h = HashMap::new();

    let f = File::open(path).map_err(|e| e.to_string())?;
    for line in BufReader::new(f).lines() {
        let l = line.map_err(|e| e.to_string())?;
        let mut ws_it = l.split_whitespace();

        let Some(mime) = ws_it.next() else { continue };

        // ignore comments
        if mime.chars().next().unwrap_or('-') == '#' {
            continue;
        }
        for ext in ws_it {
            h.insert(ext.to_string(), mime.to_string());
        }
    }
    Ok(h)
}

// strip a "?query=string" suffix, if any, from a request URL before it's used
// as a filesystem path
fn strip_query(url: &str) -> &str {
    url.split_once('?').map_or(url, |(path, _)| path)
}

// percent-decodes a single path segment (no '/' in the input, since callers
// split on it first). Rejects a decoded '/' or NUL byte, and any malformed
// escape, so a segment like "%2e%2e" (encoded "..") or "a%2fb" (encoded
// "a/b") can't smuggle a fake path separator or terminator past
// clamp_segments()'s lexical ".." handling, which runs on the *decoded*
// value.
fn percent_decode_segment(s: &str) -> Result<String, Box<dyn Error>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .ok_or_else(|| format!("fileserver: malformed percent-encoding in {s:?}"))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| format!("fileserver: malformed percent-encoding in {s:?}"))?;
            if byte == b'/' || byte == 0 {
                return Err(format!("fileserver: invalid percent-encoded byte in {s:?}").into());
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("fileserver: invalid UTF-8 in {s:?}").into())
}

// split a request url into the list of percent-decoded path segments a file
// lookup should use, clamping ".." (encoded or not) so it can never walk
// above the root: this is purely lexical (no filesystem access, no link
// resolution), so it says nothing about symlinks -- that's handled
// separately, via walk_segments()/open_regular_at(). Segments are decoded here (rather
// than left percent-encoded) so a link this vmod generates in a directory
// listing resolves to the same file it was generated from.
fn clamp_segments(url: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let mut components = Vec::new();
    for raw in url.split('/') {
        if raw.is_empty() {
            continue;
        }
        let decoded = percent_decode_segment(raw)?;
        match decoded.as_str() {
            "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(decoded),
        }
    }
    Ok(components)
}

// joins already-decoded path segments onto root_path
fn join_segments(root_path: &str, segments: &[String]) -> PathBuf {
    assert_ne!(root_path, "");

    let mut complete_path = String::from(root_path);
    for c in segments {
        complete_path.push('/');
        complete_path.push_str(c);
    }
    PathBuf::from(complete_path)
}

// given root_path and url, assemble the two so that the final path is still
// inside root_path -- a thin wrapper over clamp_segments()+join_segments(),
// kept around for the tests below since get_response() needs the segments
// and the path separately and so doesn't go through it
#[cfg(test)]
fn assemble_file_path(root_path: &str, url: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(join_segments(root_path, &clamp_segments(url)?))
}

// re-encodes decoded path segments back into a normalized, safe absolute
// URL path (e.g. for a redirect Location or a directory listing's "Index
// of ..." title): collapses whatever "//", ".", or ".." the original
// request had (clamp_segments already did that), and re-escapes reserved
// characters per segment so it can't be mistaken for a scheme-relative URL
// (e.g. a request for a directory literally named "evil.example.com" must
// not turn into a Location starting with "//evil.example.com")
fn normalized_url_path(segments: &[String]) -> String {
    let mut out = String::from("/");
    for seg in segments {
        out.push_str(&url_escape(seg));
        out.push('/');
    }
    out
}

// same as normalized_url_path(), but not percent-encoded: for display only
// (e.g. a generated listing's "Index of ..." title), never for a Location
// or an href -- entry names in that same listing are shown decoded, so the
// title should match rather than showing e.g. "has%20space" next to "has space.txt"
#[cfg(feature = "autoindex")]
fn decoded_url_path(segments: &[String]) -> String {
    let mut out = String::from("/");
    for seg in segments {
        out.push_str(seg);
        out.push('/');
    }
    out
}

// percent-encode a filename for use in an href or URL path, so names with
// spaces or other reserved characters don't produce a broken link; '/' is
// left alone since it's only ever used here as a trailing directory marker
// or a path separator we inserted ourselves, never as part of a decoded
// segment (percent_decode_segment rejects a decoded '/' inside a segment)
fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

// walk `segments` one at a time relative to `root`, opening each
// intermediate segment as a directory, using openat(2)'s O_NOFOLLOW at every
// hop (via the `openat` crate) so a symlink anywhere along the way --
// whether it points inside or outside root, we don't distinguish -- makes
// the lookup fail instead of being followed. The final segment (or "."
// if `segments` is empty) is handed to `open_last`, relative to wherever
// the walk ended up, so callers can open it as either a file or a directory
fn walk_segments<T>(
    root: &Dir,
    segments: &[String],
    open_last: impl FnOnce(&Dir, &str) -> std::io::Result<T>,
) -> std::io::Result<T> {
    match segments {
        [] => open_last(root, "."),
        [last] => open_last(root, last.as_str()),
        [dirs @ .., last] => {
            let mut cur = root.sub_dir(dirs[0].as_str())?;
            for d in &dirs[1..] {
                cur = cur.sub_dir(d.as_str())?;
            }
            open_last(&cur, last.as_str())
        }
    }
}

// opens `name` with O_NONBLOCK (matching nginx's ngx_open_and_stat_file:
// verified against a real nginx via strace, it does exactly this), then
// fstats the fd we actually opened and refuses anything that's neither a
// regular file nor a directory: opening a FIFO with no writer on the
// other end (or certain char/block devices) blocks the calling thread
// forever without O_NONBLOCK, so a request for a FIFO placed in the root
// (directly, or as an index_file candidate) must not reach a blocking
// open. O_NONBLOCK has no effect on a regular file's later reads, so
// there's no need to clear it once we know what we've got. Checking the
// type via the fd we're about to use (rather than a separate stat-by-name
// call first) also closes the TOCTOU a stat-then-open sequence would have
// between the two: this is the actual file we open, not whatever a
// second, later lookup of the same name happens to find
fn open_regular_at(dir: &Dir, name: &str, follow: bool) -> std::io::Result<File> {
    let cname = std::ffi::CString::new(name)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // when !follow, a symlink is deliberately NOT rejected here: O_NOFOLLOW
    // already makes this fail on its own, with its own (tested) error kind
    // -- this check only needs to keep FIFOs/sockets/devices from being
    // opened. follow=true (follow_links=true) omits O_NOFOLLOW so a
    // symlink resolves instead, matching that mode's semantics everywhere
    // else in this vmod
    let mut flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    let fd = unsafe { libc::openat(dir.as_raw_fd(), cname.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let md = file.metadata()?;
    if !md.is_file() && !md.is_dir() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

// same guard as open_regular_at(), for the follow_links=true (no openat::Dir)
// code paths, which resolve paths via plain std::fs instead -- no
// O_NOFOLLOW here, since follow_links=true means symlinks should resolve
fn open_regular(path: &std::path::Path) -> std::io::Result<File> {
    let cpath = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let md = file.metadata()?;
    if !md.is_file() && !md.is_dir() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

// true if the request's if-none-match/if-modified-since means the client
// already has the current version and can be sent 304 instead of a body.
// `modified` is truncated to whole-second precision to match what
// last-modified actually sends (RFC 7232 conditional headers are second-
// granularity; comparing against the untruncated mtime would make a file
// whose mtime has a non-zero sub-second part never 304, even when the
// client correctly echoes back the exact last-modified value it was given)
fn is_not_modified(
    etag: &str,
    modified: Option<DateTime<Utc>>,
    inm: Option<&str>,
    ims: Option<&str>,
) -> bool {
    if let Some(inm) = inm {
        return inm == etag || (inm.starts_with("W/") && &inm[2..] == etag);
    }
    let Some(modified) = modified else {
        return false;
    };
    let modified = modified.trunc_subsecs(0);
    let Some(ims) = ims else {
        return false;
    };
    let Ok(t) = DateTime::parse_from_rfc2822(ims) else {
        return false;
    };
    t >= modified
}

fn generate_etag(metadata: &Metadata, modified: Option<SystemTime>) -> String {
    #[derive(Hash)]
    struct ShortMd {
        inode: u64,
        size: u64,
        modified: Option<SystemTime>,
    }

    let smd = ShortMd {
        inode: metadata.ino(),
        size: metadata.size(),
        modified,
    };
    // SipHash-1-3 with fixed keys, deterministic across restarts but not
    // collision-resistant -- fine here since an ETag is a cache-validation
    // token, not a security boundary
    let mut h = DefaultHasher::new();
    smd.hash(&mut h);
    format!("\"{}\"", h.finish())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::assemble_file_path;

    fn tc(root_path: &str, url: &str, expected: &str) {
        assert_eq!(
            assemble_file_path(root_path, url).unwrap(),
            PathBuf::from(expected)
        );
    }

    #[test]
    fn simple() {
        tc("/foo/bar", "/baz/qux", "/foo/bar/baz/qux");
    }

    #[test]
    fn simple_slash() {
        tc("/foo/bar/", "/baz/qux", "/foo/bar/baz/qux");
    }

    #[test]
    fn parent() {
        tc("/foo/bar", "/bar/../qux", "/foo/bar/qux");
    }

    #[test]
    fn too_many_parents() {
        tc("/foo/bar", "/bar/../../qux", "/foo/bar/qux");
    }

    #[test]
    fn current() {
        tc("/foo/bar", "/bar/././qux", "/foo/bar/bar/qux");
    }

    #[test]
    fn percent_decoded() {
        tc("/foo/bar", "/has%20space.txt", "/foo/bar/has space.txt");
    }

    #[test]
    fn percent_encoded_parent_is_clamped() {
        // "%2e%2e" decodes to "..", and must be clamped exactly like a
        // literal ".." -- otherwise it could walk above root_path once the
        // decoded segment reaches the filesystem
        tc("/foo/bar", "/bar/%2e%2e/qux", "/foo/bar/qux");
    }

    #[test]
    fn percent_encoded_slash_is_rejected() {
        // "%2f" decodes to '/', which would let a single URL segment smuggle
        // in an extra path separator
        assert!(super::clamp_segments("/foo%2fbar").is_err());
    }

    #[test]
    fn percent_encoded_nul_is_rejected() {
        assert!(super::clamp_segments("/foo%00bar").is_err());
    }

    #[test]
    fn malformed_percent_encoding_is_rejected() {
        assert!(super::clamp_segments("/foo%2").is_err());
        assert!(super::clamp_segments("/foo%zz").is_err());
    }

    use super::strip_query;

    #[test]
    fn strip_query_removes_suffix() {
        assert_eq!(strip_query("/app.js?v=123"), "/app.js");
    }

    #[test]
    fn strip_query_no_query_string() {
        assert_eq!(strip_query("/app.js"), "/app.js");
    }

    #[test]
    fn strip_query_empty_query_string() {
        assert_eq!(strip_query("/app.js?"), "/app.js");
    }

    #[test]
    fn strip_query_only_query_string() {
        assert_eq!(strip_query("?v=123"), "");
    }

    #[test]
    fn strip_query_multiple_question_marks() {
        assert_eq!(strip_query("/app.js?v=123?extra=456"), "/app.js");
    }

    use super::build_mime_dict;
    #[test]
    fn duplicate_extension_uses_last_value() {
        let h = build_mime_dict("tests/dup1.types").unwrap();
        assert_eq!(h["txt"], "application/text");
        assert_eq!(h["pdf"], "application/pdf");
    }

    #[test]
    fn good() {
        let h = build_mime_dict("tests/good1.types").unwrap();
        assert_eq!(h["t1"], "type1");
        assert_eq!(h["T1"], "type1");
        assert_eq!(h["t3"], "type3");
        assert_eq!(h["ty3"], "type3");
        assert_eq!(h["T3"], "type3");
        assert_eq!(h.get("t2"), None);
    }

    use super::validate_index_file_name;

    #[test]
    fn index_file_name_bare_filename_is_valid() {
        assert!(validate_index_file_name("index.html").is_ok());
    }

    #[test]
    fn index_file_name_rejects_subdirectory() {
        assert!(validate_index_file_name("sub/index.html").is_err());
    }

    #[test]
    fn index_file_name_rejects_absolute_path() {
        assert!(validate_index_file_name("/etc/passwd").is_err());
    }

    #[test]
    fn index_file_name_rejects_empty() {
        assert!(validate_index_file_name("").is_err());
    }

    #[test]
    fn index_file_name_rejects_dot_and_dotdot() {
        assert!(validate_index_file_name(".").is_err());
        assert!(validate_index_file_name("..").is_err());
    }

    use super::sob_helper;
    use varnish::vcl::StrOrBytes;

    #[test]
    fn sob_helper_returns_the_str_for_utf8() {
        assert_eq!(sob_helper(StrOrBytes::Utf8("hello")), Some("hello"));
    }

    #[test]
    fn sob_helper_returns_none_for_non_utf8_instead_of_panicking() {
        // a header value is attacker-controlled; panicking here would abort
        // the whole worker process across the extern "C" backend boundary
        assert_eq!(sob_helper(StrOrBytes::Bytes(&[0xff, 0xfe])), None);
    }

    use super::url_escape;

    #[test]
    fn url_escape_leaves_safe_chars_alone() {
        assert_eq!(url_escape("abc-123_.~/"), "abc-123_.~/");
    }

    #[test]
    fn url_escape_encodes_space_and_reserved_chars() {
        assert_eq!(url_escape("a b#c?d"), "a%20b%23c%3Fd");
    }

    use super::normalized_url_path;

    #[test]
    fn normalized_url_path_root() {
        assert_eq!(normalized_url_path(&[]), "/");
    }

    #[test]
    fn normalized_url_path_nested() {
        let segments = ["a".to_string(), "has space".to_string()];
        assert_eq!(normalized_url_path(&segments), "/a/has%20space/");
    }

    #[cfg(feature = "autoindex")]
    use super::html_escape;

    #[cfg(feature = "autoindex")]
    #[test]
    fn html_escape_escapes_all_special_chars() {
        assert_eq!(
            html_escape("<a href=\"x\">&amp;</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;amp;&lt;/a&gt;"
        );
    }

    #[cfg(feature = "autoindex")]
    #[test]
    fn html_escape_neutralizes_control_characters() {
        // a filename can legally contain an ESC byte (or other C0/C1
        // controls); it must never reach the listing's HTML text raw, or
        // anyone viewing the response through a terminal gets arbitrary
        // escape sequences executed (screen clears, hidden text, etc.)
        assert_eq!(
            html_escape("evil\x1b[31mred\x1b[0m"),
            "evil\u{FFFD}[31mred\u{FFFD}[0m"
        );
        // DEL and a C1 control (both still `char::is_control()`)
        assert_eq!(html_escape("a\x7fb\u{0085}c"), "a\u{FFFD}b\u{FFFD}c");
        // ordinary text is untouched
        assert_eq!(html_escape("normal.txt"), "normal.txt");
    }

    #[cfg(feature = "autoindex")]
    #[test]
    fn html_escape_neutralizes_bidi_and_invisible_chars() {
        // U+202E (RIGHT-TO-LEFT OVERRIDE) is not `char::is_control()`, but
        // can make a browser *display* "gpj.exe" as "exe.jpg" -- the classic
        // filename-spoofing trick, and it works in any renderer, not just a
        // terminal, so it needs its own check alongside is_control()
        assert_eq!(html_escape("evil\u{202e}gpj.exe"), "evil\u{FFFD}gpj.exe");
        // zero-width space, used to hide/split text
        assert_eq!(html_escape("a\u{200b}b"), "a\u{FFFD}b");
        // a BOM inside a name
        assert_eq!(html_escape("a\u{FEFF}b"), "a\u{FFFD}b");
        // Arabic letter mark: the third implicit bidi mark, alongside LRM/RLM
        assert_eq!(html_escape("a\u{061c}b"), "a\u{FFFD}b");
        // line/paragraph separator: could fracture the <pre> column layout
        assert_eq!(html_escape("a\u{2028}b\u{2029}c"), "a\u{FFFD}b\u{FFFD}c");
        // soft hyphen, Mongolian vowel separator, a variation selector
        assert_eq!(
            html_escape("a\u{00ad}b\u{180e}c\u{fe0f}d"),
            "a\u{FFFD}b\u{FFFD}c\u{FFFD}d"
        );
        // Unicode "tag" block: invisible "ASCII smuggling" characters
        assert_eq!(html_escape("a\u{e0001}b\u{e0061}c"), "a\u{FFFD}b\u{FFFD}c");
    }

    #[cfg(feature = "autoindex")]
    use super::human_readable_size;

    #[cfg(feature = "autoindex")]
    #[test]
    fn human_readable_size_bytes() {
        assert_eq!(human_readable_size(0), "0B");
        assert_eq!(human_readable_size(1023), "1023B");
    }

    #[cfg(feature = "autoindex")]
    #[test]
    fn human_readable_size_kilobytes() {
        assert_eq!(human_readable_size(2048), "2.0K");
    }

    #[cfg(feature = "autoindex")]
    use super::pick_autoindex_format;

    #[cfg(feature = "autoindex")]
    #[test]
    fn pick_autoindex_format_defaults_to_html() {
        assert!(matches!(
            pick_autoindex_format(None),
            super::AutoindexFormat::Html
        ));
        assert!(matches!(
            pick_autoindex_format(Some("text/plain")),
            super::AutoindexFormat::Html
        ));
    }

    #[cfg(feature = "autoindex")]
    #[test]
    fn pick_autoindex_format_matches_json_and_yaml() {
        assert!(matches!(
            pick_autoindex_format(Some("application/json")),
            super::AutoindexFormat::Json
        ));
        assert!(matches!(
            pick_autoindex_format(Some("application/x-yaml")),
            super::AutoindexFormat::Yaml
        ));
    }

    use super::is_not_modified;
    use chrono::DateTime;

    #[test]
    fn is_not_modified_matches_etag() {
        assert!(is_not_modified("\"abc\"", None, Some("\"abc\""), None));
    }

    #[test]
    fn is_not_modified_matches_weak_etag() {
        assert!(is_not_modified("\"abc\"", None, Some("W/\"abc\""), None));
    }

    #[test]
    fn is_not_modified_mismatched_etag_no_ims() {
        assert!(!is_not_modified("\"abc\"", None, Some("\"wrong\""), None));
    }

    #[test]
    fn is_not_modified_ims_matches_after_truncating_subseconds() {
        // the mtime has a non-zero sub-second component (as most real mtimes
        // do), but last-modified only ever sends whole seconds -- a client
        // correctly echoing back that exact (truncated) value must still 304
        let modified = DateTime::from_timestamp(1_000, 500_000_000).unwrap();
        let ims = DateTime::from_timestamp(1_000, 0).unwrap().to_rfc2822();
        assert!(is_not_modified(
            "\"etag\"",
            Some(modified),
            None,
            Some(&ims)
        ));
    }

    #[test]
    fn is_not_modified_ims_older_than_modified() {
        let modified = DateTime::from_timestamp(1_000, 0).unwrap();
        let ims = DateTime::from_timestamp(999, 0).unwrap().to_rfc2822();
        assert!(!is_not_modified(
            "\"etag\"",
            Some(modified),
            None,
            Some(&ims)
        ));
    }
}
