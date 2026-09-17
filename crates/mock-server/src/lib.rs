//! Local HTTP mock server for tests and benches: deterministic responses,
//! optional artificial delay, custom headers, and per-path hit counting.
//!
//! Kept dependency-light on purpose (raw tokio, no HTTP framework): it backs
//! `cargo bench` runs where the mock must never be the bottleneck.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A canned response for one route.
#[derive(Debug, Clone)]
pub struct MockResponse {
    status: u16,
    body: Value,
    delay: Duration,
    headers: Vec<(String, String)>,
}

impl MockResponse {
    /// A JSON response with the given status.
    #[must_use]
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            body,
            delay: Duration::ZERO,
            headers: vec![],
        }
    }

    /// Delays the response, simulating server processing time.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Adds a response header (e.g. `Server-Timing`).
    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// Configures the routes before spawning the server.
#[derive(Debug, Default)]
pub struct MockServerBuilder {
    routes: HashMap<String, MockResponse>,
}

impl MockServerBuilder {
    /// Registers a response for an exact path; unknown paths get a 404.
    #[must_use]
    pub fn route(mut self, path: &str, response: MockResponse) -> Self {
        self.routes.insert(path.to_string(), response);
        self
    }

    /// Binds an ephemeral local port and serves the routes in a background
    /// task until the returned [`MockServer`] is dropped.
    ///
    /// # Panics
    ///
    /// Panics when no local port can be bound (nothing sensible to do in a
    /// test or bench without one).
    pub async fn spawn(self) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local mock port");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));

        let routes = Arc::new(self.routes);
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let counted = hits.clone();

        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                let hits = counted.clone();
                tokio::spawn(async move {
                    serve_connection(socket, &routes, &hits).await;
                });
            }
        });

        MockServer { base_url, hits }
    }
}

/// A running mock; dropping it releases the port.
#[derive(Debug)]
pub struct MockServer {
    base_url: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl MockServer {
    #[must_use]
    pub fn builder() -> MockServerBuilder {
        MockServerBuilder::default()
    }

    /// Base URL of the server, e.g. `http://127.0.0.1:49152`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// How many requests hit `path` so far.
    ///
    /// # Panics
    ///
    /// Panics if the internal counter lock is poisoned (a panicking mock
    /// connection task), which would invalidate the test anyway.
    #[must_use]
    pub fn hits(&self, path: &str) -> usize {
        self.hits
            .lock()
            .expect("hit counter lock")
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

/// Reads one HTTP/1.1 request and writes the configured response.
async fn serve_connection(
    mut socket: tokio::net::TcpStream,
    routes: &HashMap<String, MockResponse>,
    hits: &Mutex<HashMap<String, usize>>,
) {
    let Some(head) = read_request(&mut socket).await else {
        return;
    };
    let path = request_path(&head);

    if let Ok(mut counters) = hits.lock() {
        *counters.entry(path.clone()).or_default() += 1;
    }

    let not_found = MockResponse::json(404, serde_json::json!({"error": "not found"}));
    let response = routes.get(&path).unwrap_or(&not_found);
    write_response(&mut socket, response).await;
}

/// Reads one full HTTP/1.1 request (headers plus the declared body) and
/// returns the header block, or `None` when the peer hangs up early.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    // Drain the body so the client is never cut off mid-send.
    while buf.len() < header_end + content_length(&head) {
        let n = match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(head)
}

/// The declared `Content-Length`, or 0 when absent or unparseable.
fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
}

/// The request path from the request line, defaulting to `/`.
fn request_path(head: &str) -> String {
    head.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string()
}

/// Writes the canned response (after its configured delay) and closes.
async fn write_response(socket: &mut tokio::net::TcpStream, response: &MockResponse) {
    if response.delay > Duration::ZERO {
        tokio::time::sleep(response.delay).await;
    }

    let body = serde_json::to_vec(&response.body).unwrap_or_default();
    let mut head = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(&body).await;
    let _ = socket.shutdown().await;
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    async fn get(url: &str) -> (u16, Value, HashMap<String, String>) {
        let response = reqwest::get(url).await.expect("request");
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body, headers)
    }

    #[tokio::test]
    async fn serves_the_configured_status_and_body() {
        let server = MockServer::builder()
            .route("/price", MockResponse::json(200, json!({"unitPrice": 1.5})))
            .spawn()
            .await;

        let (status, body, _) = get(&format!("{}/price", server.base_url())).await;

        assert_eq!(status, 200);
        assert_eq!(body, json!({"unitPrice": 1.5}));
    }

    #[tokio::test]
    async fn unknown_paths_get_a_404() {
        let server = MockServer::builder().spawn().await;
        let (status, _, _) = get(&format!("{}/nope", server.base_url())).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn custom_headers_are_sent() {
        let server = MockServer::builder()
            .route(
                "/timed",
                MockResponse::json(200, json!({}))
                    .with_header("Server-Timing", "total;dur=5,db;dur=3"),
            )
            .spawn()
            .await;

        let (_, _, headers) = get(&format!("{}/timed", server.base_url())).await;

        assert_eq!(
            headers.get("server-timing").map(String::as_str),
            Some("total;dur=5,db;dur=3")
        );
    }

    #[tokio::test]
    async fn delay_holds_the_response_back() {
        let server = MockServer::builder()
            .route(
                "/slow",
                MockResponse::json(200, json!({})).with_delay(Duration::from_millis(80)),
            )
            .spawn()
            .await;

        let start = Instant::now();
        let (status, _, _) = get(&format!("{}/slow", server.base_url())).await;

        assert_eq!(status, 200);
        assert!(
            start.elapsed() >= Duration::from_millis(80),
            "the configured delay must be honored"
        );
    }

    #[tokio::test]
    async fn hits_are_counted_per_path() {
        let server = MockServer::builder()
            .route("/a", MockResponse::json(200, json!({})))
            .spawn()
            .await;
        let url = format!("{}/a", server.base_url());

        get(&url).await;
        get(&url).await;

        assert_eq!(server.hits("/a"), 2);
        assert_eq!(server.hits("/other"), 0);
    }
}
