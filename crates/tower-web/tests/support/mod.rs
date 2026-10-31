//! A tiny local mock HTTP server for `tower-web`'s enterprise-SSO route tests —
//! no live network, standing in for a real IdP's discovery/token/JWKS
//! endpoints.
//!
//! `tower-trackers`'s own harness (`TestServer`/`MockResponse` in
//! `crates/tower-trackers/src/test_support.rs`) is crate-private, and
//! `of-auth`'s own copy (`crates/of-auth/tests/support/mod.rs`, built for
//! Task 2's recorded-fixture OIDC tests) is equally unreachable from an
//! integration test in `crates/tower-web/tests/` — both are compiled into a
//! different crate's test binary. This is `tower-web`'s own small equivalent,
//! structured the same way (and, for the mock server and JWT-signing pieces,
//! copied near-verbatim from `of-auth`'s, since the shape a fixture IdP needs
//! to present does not change crate to crate).

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl MockResponse {
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: body.into(),
        }
    }
}

#[derive(Default)]
struct State {
    responses: VecDeque<MockResponse>,
    requests: Vec<CapturedRequest>,
}

pub struct TestServer {
    pub base_url: String,
    state: Arc<Mutex<State>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("server addr");
        let base_url = format!("http://{}", addr);
        let state = Arc::new(Mutex::new(State::default()));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let state_for_task = Arc::clone(&state);

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = match accepted {
                            Ok(value) => value,
                            Err(_) => break,
                        };
                        if handle_connection(&mut stream, &state_for_task).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self {
            base_url,
            state,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        }
    }

    pub fn push(&self, response: MockResponse) {
        self.state
            .lock()
            .expect("test server state")
            .responses
            .push_back(response);
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.state
            .lock()
            .expect("test server state")
            .requests
            .clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    state: &Arc<Mutex<State>>,
) -> std::io::Result<()> {
    let request = read_request(stream).await?;
    let response = {
        let mut state = state.lock().expect("test server state");
        state.requests.push(request);
        state
            .responses
            .pop_front()
            .unwrap_or_else(|| MockResponse::text(500, "test server ran out of queued responses"))
    };
    write_response(stream, response).await
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
    let mut buffer = Vec::new();
    let mut header_end = None;

    while header_end.is_none() {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        header_end = find_header_end(&buffer);
    }

    let header_end = header_end.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "request ended before headers",
        )
    })?;
    let header_bytes = &buffer[..header_end];
    let mut body = buffer[header_end + 4..].to_vec();
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "request missing request line",
        )
    })?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let normalized_name = name.trim().to_ascii_lowercase();
            let trimmed_value = value.trim().to_string();
            if normalized_name == "content-length" {
                content_length = trimmed_value.parse().unwrap_or(0);
            }
            headers.insert(normalized_name, trimmed_value);
        }
    }

    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "request ended before body completed",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(CapturedRequest {
        method,
        path,
        headers,
        body,
    })
}

async fn write_response(stream: &mut TcpStream, response: MockResponse) -> std::io::Result<()> {
    let reason = reason_phrase(response.status);
    let mut bytes = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    )
    .into_bytes();

    for (name, value) in response.headers {
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(response.body.as_bytes());
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

// ---- fixture RSA keypair, generated once for these tests only. Never used
// for anything but signing/verifying a test `id_token` in-process. Same key
// `of-auth`'s own `tests/support/mod.rs` uses — there is nothing sensitive
// about sharing a throwaway test fixture across two crates' test suites.

pub const TEST_KID: &str = "test-signing-key-1";

const TEST_RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC6BuSeN0yRbnrr
b6j1UJ8Lr91NL2Cgt0Cbr10Ig5PSL4L1LHuUjqmc+h9kJdtcih/jtw24xVcRYBzD
fZ0HlTEFDVnI2XYUljvunSjs5khe1SkdM17fXPP6v1e4yA3900TsRZJupOiJMuOK
vTE9Xx8ytmcn6/JFKtKenNRB0Og9ULMvZu3v004SZJd4fpriBOI+AppjAGlwez5v
wzN5GakcZ3XHd5zC9N6UWVXypykQ7ws0NHbHobsYqSm7pm9YT25u0D+Uzrf1FoDq
J8rJ5QjR3IrANidMn1rUxYylkHUqbtbxFKQxykbhh3/vbXNJyRpKf7fCuiewnHXA
KNZ1JewhAgMBAAECggEAU0R4mtVX4ZUZUj9F2qC+wEV1AnKdhvLf6ZACTahPx3pa
3RGPM3z0MP7IhFRpry9ofM5YRweWJIHn/h1A578BFSjXso6cSzTAGNuiEQA3DrPN
VnPDGKoLz4ZMZrqtgJtLs5KkrAAG0jrEHTr4SmdEmLeKzxTO+eTkJ/k9DUTMX3z2
+urJRm5AQU93FWnnUXMUafHSr6gRBIYBjgRnTQ7Aex/+FSyW9AbrSCb7oBlOwADv
NMnTUEduL97UUdk0xqFUBwDXhJYg4U2Rftb9p5mLPEN8pMk1QGelyPdyKVC7tY/7
C4m9vHjZAMLRKXRHfRDo8omuwhqm4EK8Edb3SEp+3QKBgQDraf8nG93y/vU+73Ao
EHQIkxsHq2RWD4VS+TBfY3EOQDGkzfkfdykkoktXpulQYInIMhblCQW0MRAaqHHz
ctqWGSvk9mB6apwqOEG5S8n2GLpPm5KdiqvPJMv/JUFFXbhAqw4jiVqy+8YKUJl0
8o4VNKW7ozDOdFO4UkWU5xzGRwKBgQDKS0+jAK22L/KgxtVZ/x/2PSVV8tB053Cn
CUwWH/Sr6dQeRCVTrDXZmM/05XtXS15QO1P2YX87t20KE8+xZtACCXMc6rSrjj5y
05RvCdrIzx5viUt1W5iKifEGiy8N9GNLGLfEZu11E/K2zGDGPfROgjBeXwC2rr09
Uum2e5AmVwKBgElyfZ/XCu1IbH2hOI3XbExMkS9YYuqS1xbnFhd8sAYxMwvnE2Wk
yNpcJEOJmNtx8yrZrdjxcq0gbZTTnxHEcLxJyC8cS0eGQYjOmnrUUYONfXte32R1
olrzcQ3+spmQvu62L6gYr4qOEOCg+u/IyVmGXnrnVE/lbUVhrcHiRVD7AoGAGqEN
S6TEOS5YnwdtgFpQJ8bmykibXjg1IRfdNzBfsd2m+ZD45OnPcORnw5INyXD3alJU
/CLbb832gZQYC/8/tHTv/Ud8HvUrjUwCxxciALsbA42sLDexfdMosjbSK+EWzQTk
8+qkqXvFwIBo4M+5ADitC08wNdwMtyzZ7RaY5CMCgYEA0JnuNngLwFa0csZDAQJR
FR14Febb3z1Oha/nnVcVWF3lSyCYXO7kfh7fZYhCFVOthDovUU9WuZJ5YFaXQOxF
hlX+mooE4RiEN2pIzc1+hbP87DvpA6frTrGPLjyXGAk7/XWjbYRJ/EhfDhPB/iuD
ynzJxIA2o6yQD6H9ChWSA+8=
-----END PRIVATE KEY-----
";

const TEST_RSA_N: &str = "ugbknjdMkW5662-o9VCfC6_dTS9goLdAm69dCIOT0i-C9Sx7lI6pnPofZCXbXIof47cNuMVXEWAcw32dB5UxBQ1ZyNl2FJY77p0o7OZIXtUpHTNe31zz-r9XuMgN_dNE7EWSbqToiTLjir0xPV8fMrZnJ-vyRSrSnpzUQdDoPVCzL2bt79NOEmSXeH6a4gTiPgKaYwBpcHs-b8MzeRmpHGd1x3ecwvTelFlV8qcpEO8LNDR2x6G7GKkpu6ZvWE9ubtA_lM639RaA6ifKyeUI0dyKwDYnTJ9a1MWMpZB1Km7W8RSkMcpG4Yd_721zSckaSn-3wronsJx1wCjWdSXsIQ";
const TEST_RSA_E: &str = "AQAB";

/// A discovery document pointing every endpoint at `server`'s mock routes.
pub fn discovery_document(base_url: &str) -> serde_json::Value {
    serde_json::json!({
        "issuer": base_url,
        "authorization_endpoint": format!("{base_url}/authorize"),
        "token_endpoint": format!("{base_url}/token"),
        "jwks_uri": format!("{base_url}/jwks"),
    })
}

/// A JWKS document containing only the fixture key above, under `TEST_KID`.
pub fn jwks_document() -> serde_json::Value {
    serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "kid": TEST_KID,
            "alg": "RS256",
            "n": TEST_RSA_N,
            "e": TEST_RSA_E,
        }]
    })
}

/// Signs an arbitrary claim set as an RS256 `id_token` under the fixture key.
pub fn sign_id_token(claims: &serde_json::Value) -> String {
    let key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY_PEM.as_bytes())
        .expect("fixture RSA key parses");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_KID.to_string());
    jsonwebtoken::encode(&header, claims, &key).expect("signing a fixture id_token")
}

/// A ready-to-use fixture IdP: discovery + token-exchange + JWKS all wired
/// to one `TestServer`, so a test only has to say what `id_token` claims it
/// wants back.
///
/// Queues, in order: the discovery document (served once, at connection-bind
/// time, by `PUT .../sso/connection`), the JWKS document (served once, at
/// first `verify_id_token`, and cached after that — see
/// `otto_auth::oidc::fetch_jwks`), and the token-exchange response naming
/// `id_token`.
pub struct FixtureIdp {
    pub server: TestServer,
    /// `otto_auth::oidc`'s JWKS cache is process-wide and keyed by `jwks_uri`
    /// (`JWKS_CACHE_TTL` = 5 minutes) — the *second* `verify_id_token` call
    /// against the same connection serves the cache and never touches this
    /// mock server again. [`Self::push_token_response`] tracks that so it
    /// only queues a JWKS response the first time it is needed; pushing one
    /// on every call would leave stale, never-popped responses sitting in
    /// front of whatever the *next* request actually expects, since this
    /// mock server serves its queue strictly in order regardless of path.
    jwks_served: std::cell::Cell<bool>,
}

impl FixtureIdp {
    /// Start a fixture IdP and queue its discovery response. The caller
    /// still has to push a token-exchange response (via
    /// [`Self::push_token_response`]) once it knows what `id_token` claims
    /// this attempt should carry — those differ per test.
    pub async fn start() -> Self {
        let server = TestServer::start().await;
        server.push(MockResponse::json(
            200,
            discovery_document(&server.base_url),
        ));
        Self {
            server,
            jwks_served: std::cell::Cell::new(false),
        }
    }

    pub fn discovery(&self) -> serde_json::Value {
        discovery_document(&self.server.base_url)
    }

    /// Queue the token endpoint's response, and the JWKS document behind it
    /// only on the first call for this fixture — see the field doc comment.
    pub fn push_token_response(&self, id_token: &str) {
        self.server.push(MockResponse::json(
            200,
            serde_json::json!({
                "access_token": "at-1",
                "token_type": "Bearer",
                "expires_in": 3600,
                "id_token": id_token,
            }),
        ));
        if !self.jwks_served.replace(true) {
            self.server.push(MockResponse::json(200, jwks_document()));
        }
    }
}
