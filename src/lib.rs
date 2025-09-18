use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::error::Error;
use std::fs::{File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Take};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use varnish::run_vtc_tests;
use varnish::vcl::{Backend, Ctx, LogTag, StrOrBytes, VclBackend, VclResponse, VclResult};

run_vtc_tests!("tests/*.vtc");

#[varnish::vmod]
mod fileserver {
    use std::error::Error;

    use varnish::ffi::VCL_BACKEND;
    use varnish::vcl::{Backend, Ctx};

    use super::root;
    use crate::{build_mime_dict, FileBackend};

    // Rust implementation of the VCC object, it mirrors what happens in C, except
    // for a couple of points:
    // - we create and return a Rust object, instead of a void pointer
    // - new() returns a Result, leaving the error handling to varnish-rs
    impl root {
        pub fn new(
            ctx: &mut Ctx,
            #[vcl_name] name: &str,
            path: &str,
            mime_db: Option<&str>,
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

            let backend = Backend::new(
                ctx,
                "fileserver",
                name,
                FileBackend {
                    mimes,
                    path: path.to_string(),
                },
                false,
            )?;
            Ok(root { backend })
        }

        pub unsafe fn backend(&self, _ctx: &Ctx) -> VCL_BACKEND {
            self.backend.vcl_ptr()
        }
    }
}

// root is the Rust implement of the VCC definition (in vmod.vcc)
// it only contains backend, which wraps a FileBackend, and
// handles response body creation with a FileTransfer
#[allow(non_camel_case_types)]
struct root {
    backend: Backend<FileBackend, FileTransfer>,
}

struct FileBackend {
    path: String,                           // top directory of our backend
    mimes: Option<HashMap<String, String>>, // a hashmap linking extensions to maps (optional)
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
        // we know that bereq and bereq_url, so we can just unwrap the options
        let bereq = ctx.http_bereq.as_ref().unwrap();
        let bereq_url = sob_helper(bereq.url().unwrap());

        // combine root and url into something that's hopefully safe
        let path = assemble_file_path(&self.path, bereq_url);
        ctx.log(
            LogTag::Debug,
            format!("fileserver: file on disk: {}", path.display()),
        );

        // reset the bereq lifetime, otherwise we couldn't use ctx in the line above
        // yes, it feels weird at first, but it's for our own good
        let bereq = ctx.http_bereq.as_ref().unwrap();

        // let's start building our response
        let beresp = ctx.http_beresp.as_mut().unwrap();

        // open the file and get some metadata
        let f = File::open(&path).map_err(|e| e.to_string())?;
        let metadata: Metadata = f.metadata().map_err(|e| e.to_string())?;
        let cl = metadata.len();
        let modified: DateTime<Utc> = DateTime::from(metadata.modified().unwrap());
        let etag = generate_etag(&metadata);

        // can we avoid sending a body?
        let mut is_304 = false;
        if let Some(inm) = bereq.header("if-none-match").map(sob_helper) {
            if inm == etag || (inm.starts_with("W/") && inm[2..] == etag) {
                is_304 = true;
            }
        } else if let Some(ims) = bereq.header("if-modified-since").map(sob_helper) {
            if let Ok(t) = DateTime::parse_from_rfc2822(ims) {
                if t > modified {
                    is_304 = true;
                }
            }
        }

        beresp.set_proto("HTTP/1.1")?;
        let mut transfer = None;
        let method = bereq.method().map(sob_helper);
        if method != Some("HEAD") && method != Some("GET") {
            // we are fairly strict in what method we accept
            beresp.set_status(405);
            return Ok(None);
        } else if is_304 {
            // 304 will save us some bandwidth
            beresp.set_status(304);
        } else {
            // "normal" request, if it's a HEAD to save a bunch of work, but if
            // it's a GET we need to add the VFP to the pipeline
            // and add a BackendResp to the priv1 field
            beresp.set_status(200);
            if method == Some("GET") {
                transfer = Some(FileTransfer {
                    // prevent reading more than expected
                    reader: BufReader::new(f).take(cl),
                });
            }
        }

        // set all the headers we can, including the content-type if we can
        beresp.set_header("content-length", &format!("{cl}"))?;
        beresp.set_header("etag", &etag)?;
        beresp.set_header(
            "last-modified",
            &modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        )?;

        // we only care about content-type if there's content
        if cl > 0 {
            // we need both and extension and a mime database
            if let (Some(ext), Some(h)) = (path.extension(), self.mimes.as_ref()) {
                if let Some(ct) = h.get(ext.to_string_lossy().as_ref()) {
                    beresp.set_header("content-type", ct)?;
                }
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

        let mime = match ws_it.next() {
            None => continue,
            Some(m) => m,
        };
        // ignore comments
        if mime.chars().next().unwrap_or('-') == '#' {
            continue;
        }
        for ext in ws_it {
            if let Some(old_mime) = h.get(ext) {
                return Err(format!(
                    "fileserver: error, in {path}, extension {ext} appears to have two types ({old_mime} and {mime})",
                )
                .into());
            }
            h.insert(ext.to_string(), mime.to_string());
        }
    }
    Ok(h)
}

// given root_path and url, assemble the two so that the final path is still
// inside root_path
// There's no access to the file system, and therefore no link resolution
// it can be an issue for multitenancy, beware!
fn assemble_file_path(root_path: &str, url: &str) -> PathBuf {
    assert_ne!(root_path, "");

    let url_path = PathBuf::from(url);
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

    let mut complete_path = String::from(root_path);
    for c in components {
        complete_path.push('/');
        complete_path.push_str(c);
    }
    PathBuf::from(complete_path)
}

fn generate_etag(metadata: &Metadata) -> String {
    #[derive(Hash)]
    struct ShortMd {
        inode: u64,
        size: u64,
        modified: SystemTime,
    }

    let smd = ShortMd {
        inode: metadata.ino(),
        size: metadata.size(),
        modified: metadata.modified().unwrap(),
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

    use super::build_mime_dict;
    #[test]
    fn bad() {
        assert_eq!(build_mime_dict("tests/bad1.types").err().unwrap().to_string(), "fileserver: error, in tests/bad1.types, extension txt appears to have two types (application/pdf and application/text)".to_string());
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
