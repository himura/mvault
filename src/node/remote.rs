//! Reading a mirror: a mounted directory or a web server, by relative path.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use crate::download::{Client, DownloadError, Downloaded, Progress, Request};
use crate::error::{Error, Result};
use crate::fsutil::io_error;
use crate::hash::{Hasher, Sha256};
use crate::store::BlobStore;

const BUFFER_SIZE: usize = 1024 * 1024;

/// The public tree of a mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    /// A directory, typically a read-only network mount.
    Dir(PathBuf),
    /// A web server; the URL of the root of the tree, without a trailing `/`.
    Http(String),
}

impl Remote {
    /// Reads a mirror location: an `http(s)://` URL, a `file://` URL, or a
    /// path.
    ///
    /// # Errors
    ///
    /// Returns an error for any other kind of URL.
    pub fn parse(location: &str) -> Result<Self> {
        let lower = location.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Ok(Self::Http(location.trim_end_matches('/').to_owned()));
        }
        if let Some(path) = lower
            .starts_with("file://")
            .then(|| &location["file://".len()..])
        {
            // `file:///C:/dir` names `C:/dir`.
            let drive = path.as_bytes().get(2) == Some(&b':') && path.starts_with('/');
            return Ok(Self::Dir(PathBuf::from(if drive {
                &path[1..]
            } else {
                path
            })));
        }
        if location.contains("://") || location.is_empty() {
            return Err(Error::Invalid(format!(
                "mirror {location:?} is neither a path nor a file://, http:// or https:// URL"
            )));
        }
        Ok(Self::Dir(PathBuf::from(location)))
    }

    /// Reads the small file at `path` (relative, with `/`). `None` if the
    /// mirror does not have it.
    ///
    /// # Errors
    ///
    /// Returns an error if the mirror cannot be read.
    pub fn read(&self, http: &Client, path: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Dir(dir) => {
                let file = dir.join(path);
                match fs::read(&file) {
                    Ok(content) => Ok(Some(content)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(io_error(&file)(error)),
                }
            }
            Self::Http(base) => match http.get(&format!("{base}/{path}"), &[]) {
                Ok(content) => Ok(Some(content)),
                Err(DownloadError::Status { status: 404, .. }) => Ok(None),
                Err(error) => Err(error.into()),
            },
        }
    }

    /// Copies the blob `digest` into the file `partial`, continuing with
    /// what the file already holds. `None` if the mirror does not have the
    /// blob.
    ///
    /// The returned digest is of what was written; the caller compares it.
    ///
    /// # Errors
    ///
    /// Returns an error if the mirror cannot be read or `partial` cannot be
    /// written.
    pub fn fetch_blob(
        &self,
        http: &Client,
        digest: &Sha256,
        partial: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Option<Downloaded>> {
        let relative = BlobStore::relative_path(digest);
        match self {
            Self::Dir(dir) => copy(&dir.join("blobs").join(relative), partial, progress),
            Self::Http(base) => {
                let hex = digest.to_string();
                let url = format!("{base}/blobs/sha256/{}/{hex}", &hex[..2]);
                let request = Request {
                    url: &url,
                    headers: &[],
                    size: None,
                };
                match http.download(&request, partial, progress) {
                    Ok(downloaded) => Ok(Some(downloaded)),
                    Err(DownloadError::Status { status: 404, .. }) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
        }
    }
}

impl std::fmt::Display for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir(dir) => write!(f, "{}", dir.display()),
            Self::Http(base) => write!(f, "{base}"),
        }
    }
}

/// Copies `source` to `partial`, hashing as it goes. What `partial` already
/// holds is hashed again and not copied again.
fn copy(
    source: &Path,
    partial: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<Option<Downloaded>> {
    let mut input = match File::open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(source)(error)),
    };
    let total = input.metadata().map_err(io_error(source))?.len();
    let mut output = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(partial)
        .map_err(io_error(partial))?;
    if output.metadata().map_err(io_error(partial))?.len() > total {
        output.set_len(0).map_err(io_error(partial))?;
    }

    let mut hasher = Hasher::new();
    let mut done = 0;
    let mut buffer = vec![0; BUFFER_SIZE];
    loop {
        let read = output.read(&mut buffer).map_err(io_error(partial))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
    }
    progress(Progress {
        done,
        total: Some(total),
    });
    input
        .seek(SeekFrom::Start(done))
        .map_err(io_error(source))?;
    loop {
        let read = input.read(&mut buffer).map_err(io_error(source))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(io_error(partial))?;
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(Progress {
            done,
            total: Some(total),
        });
    }
    Ok(Some(Downloaded {
        sha256: hasher.finish(),
        size: done,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::download::RetryPolicy;
    use crate::testutil::{Response, TestServer};

    fn http() -> Client {
        Client::new(RetryPolicy {
            max_failures: 1,
            ..RetryPolicy::default()
        })
    }

    /// A public tree with one snapshot file and one blob.
    fn tree(dir: &Path) -> Sha256 {
        let digest = Sha256::of(b"weights");
        let blob = dir.join("blobs").join(BlobStore::relative_path(&digest));
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(blob, b"weights").unwrap();
        fs::create_dir_all(dir.join("snapshots")).unwrap();
        fs::write(dir.join("snapshots/a.json"), b"{}").unwrap();
        digest
    }

    /// Serves the files below `dir` the way a web server would.
    fn serve(dir: &Path) -> TestServer {
        let dir = dir.to_owned();
        TestServer::start(move |request| {
            match fs::read(dir.join(request.path.trim_start_matches('/'))) {
                Ok(content) => Response::file(request, &content),
                Err(_) => Response::status(404),
            }
        })
    }

    #[test]
    fn locations_are_paths_or_urls() {
        assert_eq!(
            Remote::parse("/mnt/vault/public").unwrap(),
            Remote::Dir("/mnt/vault/public".into())
        );
        assert_eq!(
            Remote::parse("file:///mnt/vault/public").unwrap(),
            Remote::Dir("/mnt/vault/public".into())
        );
        assert_eq!(
            Remote::parse("FILE:///C:/vault").unwrap(),
            Remote::Dir("C:/vault".into())
        );
        assert_eq!(
            Remote::parse("https://mirror.example.internal/models/").unwrap(),
            Remote::Http("https://mirror.example.internal/models".into())
        );
        assert_eq!(
            Remote::parse("http://10.0.0.1:8080").unwrap(),
            Remote::Http("http://10.0.0.1:8080".into())
        );
        assert!(Remote::parse("ftp://example.com/").is_err());
        assert!(Remote::parse("").is_err());
    }

    #[test]
    fn both_kinds_read_files_and_report_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        tree(dir.path());
        let server = serve(dir.path());
        for remote in [
            Remote::Dir(dir.path().to_owned()),
            Remote::parse(&server.url("/")).unwrap(),
        ] {
            assert_eq!(
                remote.read(&http(), "snapshots/a.json").unwrap().unwrap(),
                b"{}",
                "{remote}"
            );
            assert_eq!(
                remote.read(&http(), "snapshots/missing.json").unwrap(),
                None,
                "{remote}"
            );
        }
    }

    #[test]
    fn both_kinds_fetch_blobs_and_continue_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let digest = tree(dir.path());
        let server = serve(dir.path());
        for remote in [
            Remote::Dir(dir.path().to_owned()),
            Remote::parse(&server.url("")).unwrap(),
        ] {
            let out = tempfile::tempdir().unwrap();
            let partial = out.path().join("partial");

            let mut seen = Vec::new();
            let fetched = remote
                .fetch_blob(&http(), &digest, &partial, &mut |p| seen.push(p))
                .unwrap()
                .unwrap();
            assert_eq!((fetched.sha256, fetched.size), (digest, 7), "{remote}");
            assert_eq!(fs::read(&partial).unwrap(), b"weights");
            assert_eq!(seen.last().unwrap().done, 7);

            fs::write(&partial, b"wei").unwrap();
            let continued = remote
                .fetch_blob(&http(), &digest, &partial, &mut |_| {})
                .unwrap()
                .unwrap();
            assert_eq!(continued.sha256, digest, "{remote}");
            assert_eq!(fs::read(&partial).unwrap(), b"weights");

            let missing = Sha256::of(b"missing");
            let other = out.path().join("other");
            assert_eq!(
                remote
                    .fetch_blob(&http(), &missing, &other, &mut |_| {})
                    .unwrap(),
                None
            );
        }
        let ranges: Vec<_> = server
            .requests()
            .iter()
            .filter_map(|request| request.header("range").map(str::to_owned))
            .collect();
        assert_eq!(ranges, ["bytes=3-"]);
    }

    #[test]
    fn a_partial_file_longer_than_the_blob_is_started_over() {
        let dir = tempfile::tempdir().unwrap();
        let digest = tree(dir.path());
        let partial = dir.path().join("partial");
        fs::write(&partial, b"much longer than the blob").unwrap();

        let fetched = Remote::Dir(dir.path().to_owned())
            .fetch_blob(&http(), &digest, &partial, &mut |_| {})
            .unwrap()
            .unwrap();

        assert_eq!(fetched.sha256, digest);
        assert_eq!(fs::read(&partial).unwrap(), b"weights");
    }
}
