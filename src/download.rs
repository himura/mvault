//! Downloading a file over HTTP while computing its digest.
//!
//! A download writes to a file that survives failures. Whatever that file
//! already holds is taken as the beginning of the content, and only the rest
//! is requested.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ureq::config::RedirectAuthHeaders;
use ureq::tls::{RootCerts, TlsConfig};

use crate::hash::{Hasher, Sha256};

const BUFFER_SIZE: usize = 1024 * 1024;

/// How often and how patiently a failed request is repeated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Failures in a row, without any new data in between, that end the
    /// download.
    pub max_failures: u32,
    /// Pause after the first failure; doubled after each further one.
    pub initial_delay: Duration,
    /// Upper limit of the pause.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.initial_delay
            .saturating_mul(1 << doublings)
            .min(self.max_delay)
    }
}

/// What to download.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The URL of the file.
    pub url: &'a str,
    /// Headers to send, such as `Authorization`. They are not passed on when
    /// the server redirects to another host.
    pub headers: &'a [(&'a str, &'a str)],
    /// The size of the file, if it is known in advance.
    pub size: Option<u64>,
}

/// How far a download has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes in the destination file so far.
    pub done: u64,
    /// Size of the whole file, once known.
    pub total: Option<u64>,
}

/// A completely downloaded file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downloaded {
    /// Digest of the content.
    pub sha256: Sha256,
    /// Size of the content in bytes.
    pub size: u64,
}

/// The error returned when a download does not complete.
///
/// Unless noted otherwise the destination file is kept, so that a later
/// attempt continues where this one stopped.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The destination file could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The destination file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The server answered with a status that repeating will not change.
    #[error("{url}: HTTP status {status}")]
    Status {
        /// The requested URL.
        url: String,
        /// The status code.
        status: u16,
    },
    /// The request kept failing.
    #[error("{url}: {reason}")]
    Failed {
        /// The requested URL.
        url: String,
        /// What went wrong the last time.
        reason: String,
    },
    /// The server sent more than the announced size. The destination file is
    /// emptied.
    #[error("{url}: expected {expected} bytes, got more")]
    TooLong {
        /// The requested URL.
        url: String,
        /// The size given in the request.
        expected: u64,
    },
}

/// An HTTP client for downloads.
#[derive(Debug, Clone)]
pub struct Client {
    agent: ureq::Agent,
    retry: RetryPolicy,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(RetryPolicy::default())
    }
}

/// Why one request did not complete the download.
enum Failure {
    /// Worth repeating.
    Retry(String),
    /// The server does not have the range after what the file holds, so the
    /// file is not a beginning of the content.
    Restart,
    Fatal(DownloadError),
}

/// The destination file and what is known about its content.
struct Target<'a> {
    path: &'a Path,
    file: File,
    hasher: Hasher,
    len: u64,
}

impl Target<'_> {
    fn io_error(&self, source: io::Error) -> DownloadError {
        DownloadError::Io {
            path: self.path.to_owned(),
            source,
        }
    }

    /// Hashes what the file already holds and leaves the cursor at its end.
    fn open(path: &Path) -> Result<Target<'_>, DownloadError> {
        let io_error = |source| DownloadError::Io {
            path: path.to_owned(),
            source,
        };
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_error)?;
        let mut hasher = Hasher::new();
        let mut len = 0;
        let mut buffer = vec![0; BUFFER_SIZE];
        loop {
            let read = file.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            len += read as u64;
        }
        Ok(Target {
            path,
            file,
            hasher,
            len,
        })
    }

    fn clear(&mut self) -> Result<(), DownloadError> {
        self.file
            .set_len(0)
            .and_then(|()| self.file.rewind())
            .map_err(|e| self.io_error(e))?;
        self.hasher = Hasher::new();
        self.len = 0;
        Ok(())
    }

    fn append(&mut self, data: &[u8]) -> Result<(), DownloadError> {
        self.file.write_all(data).map_err(|e| self.io_error(e))?;
        self.hasher.update(data);
        self.len += data.len() as u64;
        Ok(())
    }
}

impl Client {
    /// A client that repeats failed requests according to `retry`.
    ///
    /// Server certificates are checked against the trust store of the
    /// platform, so that a mirror with a certificate from a private authority
    /// works once that authority is installed on the machine.
    #[must_use]
    pub fn new(retry: RetryPolicy) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("mvault/", env!("CARGO_PKG_VERSION")))
            .redirect_auth_headers(RedirectAuthHeaders::SameHost)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        Self {
            agent: config.into(),
            retry,
        }
    }

    /// Downloads `request.url` into the file `dest`.
    ///
    /// If `dest` already has content, it is taken as the beginning of the file
    /// and only the rest is requested. `progress` is called before the first
    /// request and after each piece written.
    ///
    /// The digest is of what was written; comparing it with an expected one
    /// is up to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error if `dest` cannot be written, if the server refuses
    /// the request, or if the request keeps failing. See [`DownloadError`].
    pub fn download(
        &self,
        request: &Request<'_>,
        dest: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Downloaded, DownloadError> {
        let mut target = Target::open(dest)?;
        if request.size.is_some_and(|size| target.len > size) {
            target.clear()?;
        }
        progress(Progress {
            done: target.len,
            total: request.size,
        });

        let mut failures = 0;
        while request.size != Some(target.len) {
            let before = target.len;
            let reason = match self.attempt(request, &mut target, progress) {
                Ok(()) => break,
                Err(Failure::Fatal(error)) => return Err(error),
                Err(Failure::Retry(reason)) => reason,
                Err(Failure::Restart) => {
                    target.clear()?;
                    "the server does not have the rest of the partial file".to_owned()
                }
            };
            if target.len > before {
                failures = 0;
            }
            failures += 1;
            if failures >= self.retry.max_failures {
                return Err(DownloadError::Failed {
                    url: request.url.to_owned(),
                    reason,
                });
            }
            std::thread::sleep(self.retry.delay(failures));
        }
        Ok(Downloaded {
            sha256: target.hasher.finish(),
            size: target.len,
        })
    }

    /// One request for whatever `target` is missing. Returns when the server
    /// has sent everything it announced.
    fn attempt(
        &self,
        request: &Request<'_>,
        target: &mut Target<'_>,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(), Failure> {
        // Compressed transfer would make byte ranges meaningless.
        let mut call = self
            .agent
            .get(request.url)
            .header("Accept-Encoding", "identity");
        for (name, value) in request.headers {
            call = call.header(*name, *value);
        }
        if target.len > 0 {
            call = call.header("Range", format!("bytes={}-", target.len));
        }
        let response = call.call().map_err(|error| match error {
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
            | ureq::Error::Protocol(_) => Failure::Retry(error.to_string()),
            _ => Failure::Fatal(DownloadError::Failed {
                url: request.url.to_owned(),
                reason: error.to_string(),
            }),
        })?;

        let status = response.status().as_u16();
        match status {
            200 => {
                // The server ignored the range and sends everything.
                if target.len > 0 {
                    target.clear().map_err(Failure::Fatal)?;
                }
            }
            206 => {
                let start = response
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(range_start);
                if start != Some(target.len) {
                    return Err(Failure::Retry(format!(
                        "asked for bytes from {}, got another range",
                        target.len
                    )));
                }
            }
            416 if target.len > 0 => return Err(Failure::Restart),
            408 | 429 | 500..=599 => return Err(Failure::Retry(format!("HTTP status {status}"))),
            _ => {
                return Err(Failure::Fatal(DownloadError::Status {
                    url: request.url.to_owned(),
                    status,
                }));
            }
        }

        let total = request.size.or_else(|| {
            let rest = response.body().content_length()?;
            Some(target.len + rest)
        });
        let mut reader = response.into_body().into_reader();
        let mut buffer = vec![0; BUFFER_SIZE];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|e| Failure::Retry(e.to_string()))?;
            if read == 0 {
                break;
            }
            target.append(&buffer[..read]).map_err(Failure::Fatal)?;
            progress(Progress {
                done: target.len,
                total,
            });
        }

        match request.size {
            Some(size) if target.len < size => Err(Failure::Retry(format!(
                "got {} of {size} bytes",
                target.len
            ))),
            Some(size) if target.len > size => {
                target.clear().map_err(Failure::Fatal)?;
                Err(Failure::Fatal(DownloadError::TooLong {
                    url: request.url.to_owned(),
                    expected: size,
                }))
            }
            _ => Ok(()),
        }
    }
}

/// The first byte position of a `Content-Range: bytes <start>-<end>/<size>`.
fn range_start(content_range: &str) -> Option<u64> {
    let (start, _) = content_range.strip_prefix("bytes ")?.split_once('-')?;
    start.trim().parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::testutil::{Response, TestServer};

    fn client() -> Client {
        Client::new(RetryPolicy {
            max_failures: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        })
    }

    fn content() -> Vec<u8> {
        (0..=255).cycle().take(5000).collect()
    }

    fn get(
        server: &TestServer,
        dest: &Path,
        size: Option<u64>,
    ) -> Result<Downloaded, DownloadError> {
        let request = Request {
            url: &server.url("/file"),
            headers: &[],
            size,
        };
        client().download(&request, dest, &mut |_| {})
    }

    fn ranges(server: &TestServer) -> Vec<Option<String>> {
        server
            .requests()
            .iter()
            .map(|request| request.header("range").map(str::to_owned))
            .collect()
    }

    #[test]
    fn downloads_a_file_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        let server = TestServer::start({
            let body = body.clone();
            move |request| Response::file(request, &body)
        });

        let mut seen = Vec::new();
        let request = Request {
            url: &server.url("/file"),
            headers: &[("X-Test", "value")],
            size: None,
        };
        let downloaded = client()
            .download(&request, &dest, &mut |p| seen.push(p))
            .unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(downloaded.size, 5000);
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(
            seen.first(),
            Some(&Progress {
                done: 0,
                total: None
            })
        );
        assert_eq!(
            seen.last(),
            Some(&Progress {
                done: 5000,
                total: Some(5000)
            })
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/file");
        assert_eq!(requests[0].header("x-test"), Some("value"));
        assert_eq!(requests[0].header("accept-encoding"), Some("identity"));
        assert_eq!(requests[0].header("range"), None);
    }

    #[test]
    fn continues_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        fs::write(&dest, &body[..1234]).unwrap();
        let server = TestServer::start({
            let body = body.clone();
            move |request| Response::file(request, &body)
        });

        let mut seen = Vec::new();
        let request = Request {
            url: &server.url("/file"),
            headers: &[],
            size: Some(5000),
        };
        let downloaded = client()
            .download(&request, &dest, &mut |p| seen.push(p))
            .unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(ranges(&server), [Some("bytes=1234-".to_owned())]);
        assert_eq!(
            seen.first(),
            Some(&Progress {
                done: 1234,
                total: Some(5000)
            })
        );
    }

    #[test]
    fn starts_over_when_the_server_ignores_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        fs::write(&dest, b"stale beginning").unwrap();
        let server = TestServer::start({
            let body = body.clone();
            move |_| Response::ok(&body)
        });

        let downloaded = get(&server, &dest, None).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn continues_after_a_dropped_connection() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        let calls = AtomicUsize::new(0);
        let server = TestServer::start({
            let body = body.clone();
            move |request| {
                let response = Response::file(request, &body);
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    response.cut_after(2000)
                } else {
                    response
                }
            }
        });

        let downloaded = get(&server, &dest, Some(5000)).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(ranges(&server), [None, Some("bytes=2000-".to_owned())]);
    }

    #[test]
    fn repeats_after_a_server_error() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        let calls = AtomicUsize::new(0);
        let server = TestServer::start({
            let body = body.clone();
            move |request| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Response::status(503),
                1 => Response::status(429),
                _ => Response::file(request, &body),
            }
        });

        let downloaded = get(&server, &dest, None).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn gives_up_after_repeated_failures_and_keeps_the_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        fs::write(&dest, b"beginning").unwrap();
        let server = TestServer::start(|_| Response::status(503));

        let error = get(&server, &dest, None).unwrap_err();

        assert!(
            matches!(&error, DownloadError::Failed { reason, .. } if reason.contains("503")),
            "{error}"
        );
        assert_eq!(server.requests().len(), 3);
        assert_eq!(fs::read(&dest).unwrap(), b"beginning");
    }

    #[test]
    fn does_not_repeat_a_refused_request() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let server = TestServer::start(|_| Response::status(404));

        let error = get(&server, &dest, None).unwrap_err();

        assert!(
            matches!(error, DownloadError::Status { status: 404, .. }),
            "{error}"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn a_complete_partial_file_needs_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        fs::write(&dest, &body).unwrap();
        let server = TestServer::start(|_| Response::status(500));

        let downloaded = get(&server, &dest, Some(5000)).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(downloaded.size, 5000);
        assert!(server.requests().is_empty());
    }

    #[test]
    fn a_partial_file_longer_than_the_expected_size_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        fs::write(&dest, vec![7; 6000]).unwrap();
        let server = TestServer::start({
            let body = body.clone();
            move |request| Response::file(request, &body)
        });

        let downloaded = get(&server, &dest, Some(5000)).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(ranges(&server), [None]);
    }

    #[test]
    fn starts_over_when_the_server_cannot_satisfy_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        fs::write(&dest, vec![7; 6000]).unwrap();
        let server = TestServer::start({
            let body = body.clone();
            move |request| Response::file(request, &body)
        });

        let downloaded = get(&server, &dest, None).unwrap();

        assert_eq!(downloaded.sha256, Sha256::of(&body));
        assert_eq!(fs::read(&dest).unwrap(), body);
        assert_eq!(ranges(&server), [Some("bytes=6000-".to_owned()), None]);
    }

    #[test]
    fn rejects_a_body_longer_than_the_expected_size() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        let body = content();
        let server = TestServer::start({
            let body = body.clone();
            move |request| Response::file(request, &body)
        });

        let error = get(&server, &dest, Some(4000)).unwrap_err();

        assert!(
            matches!(error, DownloadError::TooLong { expected: 4000, .. }),
            "{error}"
        );
        assert_eq!(fs::read(&dest).unwrap(), b"");
    }

    #[test]
    fn repeats_when_the_range_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("partial");
        fs::write(&dest, b"beginning").unwrap();
        let server = TestServer::start(|_| {
            Response::status(206)
                .header("Content-Range", "bytes 0-3/4")
                .body(b"data")
        });

        let error = get(&server, &dest, None).unwrap_err();

        assert!(matches!(error, DownloadError::Failed { .. }), "{error}");
        assert_eq!(fs::read(&dest).unwrap(), b"beginning");
    }

    #[test]
    fn retry_delay_doubles_up_to_the_limit() {
        let policy = RetryPolicy {
            max_failures: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
        };
        let delays: Vec<u64> = (1..=5).map(|n| policy.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 5, 5]);
        assert_eq!(policy.delay(u32::MAX), Duration::from_secs(5));
    }
}
