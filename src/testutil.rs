//! A minimal HTTP server for tests, so that no test talks to a real service.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// A request as the server received it.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// Path and query, as sent.
    pub(crate) path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    /// The value of a header, by case-insensitive name.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What the server answers.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    cut_after: Option<usize>,
    without_length: bool,
}

impl Response {
    /// `200` with a body.
    pub(crate) fn ok(body: &[u8]) -> Self {
        Self::status(200).body(body)
    }

    /// A response with this status and no body.
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            cut_after: None,
            without_length: false,
        }
    }

    pub(crate) fn body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Announces the whole body but closes the connection after `bytes` of it.
    pub(crate) fn cut_after(mut self, bytes: usize) -> Self {
        self.cut_after = Some(bytes);
        self
    }

    /// Leaves out `Content-Length`, so that the body ends where the
    /// connection does.
    pub(crate) fn without_length(mut self) -> Self {
        self.without_length = true;
        self
    }

    /// Serves `content` the way a file server does: all of it, or the part
    /// asked for by a `Range: bytes=<start>-` header.
    pub(crate) fn file(request: &Request, content: &[u8]) -> Self {
        let Some(range) = request.header("range") else {
            return Self::ok(content);
        };
        let start = range
            .strip_prefix("bytes=")
            .and_then(|spec| spec.strip_suffix('-'))
            .and_then(|start| start.parse::<usize>().ok());
        match start {
            Some(start) if start < content.len() => Self::status(206)
                .header(
                    "Content-Range",
                    &format!("bytes {start}-{}/{}", content.len() - 1, content.len()),
                )
                .body(&content[start..]),
            _ => Self::status(416).header("Content-Range", &format!("bytes */{}", content.len())),
        }
    }
}

/// An HTTP server on a local port, answering every request with a handler.
///
/// One connection is served at a time and closed after its response.
#[derive(Debug)]
pub(crate) struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub(crate) fn start(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    // A client that goes away mid-exchange is not an error here.
                    let _ = serve(stream, &handler, &requests);
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// The URL of `path` (which starts with `/`) on this server.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Every request received so far, in order.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so that it sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    stream: TcpStream,
    handler: &impl Fn(&Request) -> Response,
    requests: &Mutex<Vec<Request>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let Some(path) = line.split_whitespace().nth(1).map(str::to_owned) else {
        return Ok(());
    };
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let request = Request { path, headers };
    let response = handler(&request);
    requests.lock().unwrap().push(request);

    let mut head = vec![format!("HTTP/1.1 {} Status\r\n", response.status)];
    for (name, value) in &response.headers {
        head.push(format!("{name}: {value}\r\n"));
    }
    if !response.without_length {
        head.push(format!("Content-Length: {}\r\n", response.body.len()));
    }
    head.push("Connection: close\r\n\r\n".to_owned());
    let sent = response.cut_after.unwrap_or(response.body.len());
    let mut stream = reader.into_inner();
    stream.write_all(head.concat().as_bytes())?;
    stream.write_all(&response.body[..sent])?;
    stream.flush()
}

/// A server answering each path with a fixed body. The table can be changed
/// while the server runs.
#[derive(Debug)]
pub(crate) struct Routes {
    pub(crate) server: TestServer,
    table: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl Routes {
    pub(crate) fn start() -> Self {
        let table: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>> = Arc::default();
        let server = TestServer::start({
            let table = Arc::clone(&table);
            move |request| match table.lock().unwrap().get(&request.path) {
                Some(body) => Response::file(request, body),
                None => Response::status(404),
            }
        });
        Self { server, table }
    }

    pub(crate) fn route(&self, path: &str, body: &[u8]) {
        self.table
            .lock()
            .unwrap()
            .insert(path.to_owned(), body.to_vec());
    }

    /// The base URL of the server, without a trailing `/`.
    pub(crate) fn base(&self) -> String {
        self.server.url("")
    }
}

/// Serves the files below `dir` the way a web server would.
pub(crate) fn serve_dir(dir: &std::path::Path) -> TestServer {
    let dir = dir.to_owned();
    TestServer::start(move |request| {
        match std::fs::read(dir.join(request.path.trim_start_matches('/'))) {
            Ok(content) => Response::file(request, &content),
            Err(_) => Response::status(404),
        }
    })
}
