use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs::{File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Take};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use openat::Dir;
use varnish::run_vtc_tests;
use varnish::vcl::{Backend, Ctx, LogTag, StrOrBytes, VclBackend, VclResponse, VclResult};

run_vtc_tests!("tests/*.vtc");

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

    use openat::Dir;
    use varnish::ffi::VCL_BACKEND;
    use varnish::vcl::{Backend, Ctx};

    use super::file_backend;
    use crate::{FileBackend, build_mime_dict};

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
                },
                false,
            )?;
            Ok(file_backend { backend })
        }

        /// Return the Varnish backend serving files under this object's root.
        ///
        /// - Only `GET` and `HEAD` requests are served; anything else gets a 405.
        /// - The request URL's query string, if any, is ignored when
        ///   resolving the file on disk.
        /// - A missing file returns 404; an unreadable one returns 403.
        /// - Unless `follow_links` was set on the constructor, a request
        ///   that hits a symlink anywhere in its path fails instead of
        ///   being served.
        /// - `etag`/`if-none-match` and `last-modified`/`if-modified-since`
        ///   are supported. `etag` is derived from the file's inode, size,
        ///   and modification time (if available).
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
}

// silly helper until varnish-rs provides something more ergonomic
#[expect(clippy::needless_pass_by_value)]
fn sob_helper(sob: StrOrBytes<'_>) -> &str {
    match sob {
        StrOrBytes::Bytes(_) => panic!("{sob:?} isn't a string"),
        StrOrBytes::Utf8(s) => s,
    }
}

impl VclBackend<FileTransfer> for FileBackend {
    fn get_response(&self, ctx: &mut Ctx) -> VclResult<Option<FileTransfer>> {
        // we know that bereq and bereq_url are set, so we can just expect the options
        let bereq = ctx
            .http_bereq
            .as_ref()
            .expect("bereq is set during a backend fetch");
        let bereq_url = sob_helper(bereq.url().expect("bereq always has a url"));

        // combine root and url into something that's hopefully safe. The query
        // string (if any) is not part of the filesystem path -- nginx and Apache
        // both split path from query at the HTTP-parsing layer and never consult
        // the query string for static-file lookups, so a request like
        // "/app.js?v=123" (a common cache-busting pattern) must still resolve to
        // "/app.js" on disk, not literally fail to find a file named "app.js?v=123".
        let path = assemble_file_path(&self.path, strip_query(bereq_url));
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
        let bereq_url = sob_helper(bereq.url().expect("bereq always has a url"));

        // let's start building our response
        let beresp = ctx
            .http_beresp
            .as_mut()
            .expect("beresp is set during a backend fetch");

        // reject unsupported methods before touching the filesystem
        let method = bereq.method().map(sob_helper);
        if method != Some("HEAD") && method != Some("GET") {
            // we are fairly strict in what method we accept
            beresp.set_status(405);
            return Ok(None);
        }
        let is_get = method == Some("GET");

        // open the file and get some metadata. Unless the VCL author wants
        // symlinks followed unconditionally (follow_links), walk the
        // request one non-symlink segment at a time relative to the root
        // dir we opened in root() -- a symlink anywhere along the way
        // (inside or outside the root, we don't distinguish) makes the
        // corresponding open_file/sub_dir call fail instead of following it
        let f = match &self.root_dir {
            Some(root_dir) => open_through_dir(root_dir, &clamp_segments(strip_query(bereq_url))),
            None => File::open(&path),
        };
        let f = match f {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                beresp.set_status(404);
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                beresp.set_status(403);
                return Ok(None);
            }
            Err(e) => return Err(e.to_string().into()),
        };

        let metadata: Metadata = f.metadata().map_err(|e| e.to_string())?;
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
        let mut is_304 = false;
        if let Some(inm) = bereq.header("if-none-match").map(sob_helper) {
            if inm == etag || (inm.starts_with("W/") && inm[2..] == etag) {
                is_304 = true;
            }
        } else if let Some(modified) = modified
            && let Some(ims) = bereq.header("if-modified-since").map(sob_helper)
            && let Ok(t) = DateTime::parse_from_rfc2822(ims)
            && t >= modified
        {
            is_304 = true;
        }

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
                transfer = Some(FileTransfer {
                    // prevent reading more than expected
                    reader: BufReader::new(f).take(cl),
                });
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

struct FileTransfer {
    reader: Take<BufReader<File>>,
}

impl VclResponse for FileTransfer {
    fn read(&mut self, buf: &mut [u8]) -> VclResult<usize> {
        self.reader.read(buf).map_err(|e| e.to_string().into())
    }
    fn len(&self) -> Option<usize> {
        Some(usize::try_from(self.reader.limit()).expect("casting u64 to usize"))
    }
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

// split a request url into the list of path segments a file lookup should
// use, clamping ".." so it can never walk above the root: this is purely
// lexical (no filesystem access, no link resolution), so it says nothing
// about symlinks -- that's handled separately, see open_through_dir()
fn clamp_segments(url: &str) -> Vec<&str> {
    let url_path = std::path::Path::new(url);
    let mut components = Vec::new();

    for c in url_path.components() {
        use std::path::Component::{CurDir, Normal, ParentDir, Prefix, RootDir};
        match c {
            Prefix(_) => unreachable!(),
            RootDir => {}
            CurDir => (),
            ParentDir => {
                components.pop();
            }
            Normal(s) => {
                // we can unwrap as url_path was created from a &str
                components.push(s.to_str().unwrap());
            }
        }
    }
    components
}

// given root_path and url, assemble the two so that the final path is still
// inside root_path. Used for logging and mime-type lookup, and (when
// follow_links=true) for the actual File::open -- see clamp_segments()
fn assemble_file_path(root_path: &str, url: &str) -> PathBuf {
    assert_ne!(root_path, "");

    let mut complete_path = String::from(root_path);
    for c in clamp_segments(url) {
        complete_path.push('/');
        complete_path.push_str(c);
    }
    PathBuf::from(complete_path)
}

// walk `segments` one at a time relative to `root`, opening each
// intermediate segment as a directory and the last one as a file, using
// openat(2)'s O_NOFOLLOW at every hop (via the `openat` crate) so a
// symlink anywhere along the way -- whether it points inside or outside
// root, we don't distinguish -- makes the lookup fail instead of being
// followed
fn open_through_dir(root: &Dir, segments: &[&str]) -> std::io::Result<File> {
    match segments {
        [] => root.open_file("."),
        [file] => root.open_file(*file),
        [dirs @ .., file] => {
            let mut cur = root.sub_dir(dirs[0])?;
            for d in &dirs[1..] {
                cur = cur.sub_dir(*d)?;
            }
            cur.open_file(*file)
        }
    }
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
    let mut h = DefaultHasher::new();
    smd.hash(&mut h);
    format!("\"{}\"", h.finish())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::assemble_file_path;

    fn tc(root_path: &str, url: &str, expected: &str) {
        assert_eq!(assemble_file_path(root_path, url), PathBuf::from(expected));
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
}
