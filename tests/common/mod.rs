use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use lens::find::IdsPayload;
use lens::indexer::CaptionPayload;
use lens::providers::cerebras::TokenUsage;

pub(super) struct MockServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    last_request_body: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// A content-based find router: maps a marker string (expected to appear in
/// the request body) to a canned `ids` response. The first matching marker
/// wins. If no marker matches, falls through to the `find_ids` queue or
/// `[0]`.
type FindRouter = Vec<(String, Vec<i64>)>;

#[derive(Default)]
struct MockState {
    captions: Mutex<VecDeque<String>>,
    find_ids: Mutex<VecDeque<Vec<i64>>>,
    find_router: Mutex<FindRouter>,
    find_raw: Mutex<VecDeque<String>>,
}

#[derive(Deserialize)]
struct MockChatRequest {
    response_format: Option<MockResponseFormat>,
}

#[derive(Deserialize)]
struct MockResponseFormat {
    json_schema: MockJsonSchema,
}

#[derive(Deserialize)]
struct MockJsonSchema {
    name: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MockResponse {
    Error { error: &'static str },
    Chat(MockChatResponse),
}

#[derive(Serialize)]
struct MockChatResponse {
    choices: [MockChoice; 1],
    usage: TokenUsage,
}

#[derive(Serialize)]
struct MockChoice {
    message: MockMessage,
}

#[derive(Serialize)]
struct MockMessage {
    content: String,
}

impl MockServer {
    pub(super) fn start() -> Self {
        Self::with(vec![], vec![])
    }

    pub(super) fn with(captions: Vec<&str>, find_ids: Vec<Vec<i64>>) -> Self {
        Self::build(
            captions,
            VecDeque::from(find_ids),
            Vec::new(),
            VecDeque::new(),
        )
    }

    fn build(
        captions: Vec<&str>,
        find_ids: VecDeque<Vec<i64>>,
        find_router: FindRouter,
        find_raw: VecDeque<String>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let last_request_body: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(MockState {
            captions: Mutex::new(captions.into_iter().map(str::to_string).collect()),
            find_ids: Mutex::new(find_ids),
            find_router: Mutex::new(find_router),
            find_raw: Mutex::new(find_raw),
        });
        let thread_requests = Arc::clone(&requests);
        let thread_running = Arc::clone(&running);
        let thread_state = Arc::clone(&state);
        let thread_last_body = Arc::clone(&last_request_body);
        let handle = thread::spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        handle_client(stream, &thread_state, &thread_last_body)
                            .expect("mock server handles client request");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            requests,
            last_request_body,
            running,
            handle: Some(handle),
        }
    }

    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(super) fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub(super) fn last_request_body(&self) -> Option<String> {
        self.last_request_body.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    state: &MockState,
    last_request_body: &Mutex<Option<String>>,
) -> std::io::Result<()> {
    let (path, body, headers) = read_request(&mut stream)?;
    *last_request_body.lock().unwrap() = Some(body.clone());
    if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("authorization: bearer bad-cerebras"))
    {
        return write_json(&mut stream, 401, &MockResponse::Error { error: "bad key" });
    }
    let response = match path.as_str() {
        "/chat/completions" => chat_response(&body, state),
        _ => (404, MockResponse::Error { error: "not found" }),
    };
    write_json(&mut stream, response.0, &response.1)
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0; 1024];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let header_end = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP header terminator"))?;
    let headers = String::from_utf8(buf[..header_end].to_vec())
        .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing HTTP request path"))?
        .to_string();
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing content-length header"))?
        .1
        .trim()
        .parse::<usize>()
        .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "HTTP request body ended before content-length",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8(buf[header_end..header_end + content_length].to_vec())
        .map_err(|err| Error::new(ErrorKind::InvalidData, err))?;
    Ok((path, body, headers))
}

fn chat_response(body: &str, state: &MockState) -> (u16, MockResponse) {
    let request: MockChatRequest =
        serde_json::from_str(body).expect("client request is valid JSON");
    let schema_name = request
        .response_format
        .as_ref()
        .map(|format| format.json_schema.name.as_str());
    match schema_name {
        Some("image_index") => {
            let description = state
                .captions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "mock image caption".to_string());
            (200, chat_content(caption_json(&description)))
        }
        Some("hits") => {
            let router = state.find_router.lock().unwrap();
            for (marker, ids) in router.iter() {
                if body.contains(marker) {
                    return (200, chat_content(ids_json(ids)));
                }
            }
            drop(router);

            if let Some(raw) = state.find_raw.lock().unwrap().pop_front() {
                return (200, chat_content(raw));
            }

            let ids = state
                .find_ids
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| vec![0]);
            (200, chat_content(ids_json(&ids)))
        }
        _ => (200, chat_content("ok".to_string())),
    }
}

fn caption_json(description: &str) -> String {
    serde_json::to_string(&CaptionPayload {
        description: description.to_string(),
        filename: "mock-image".to_string(),
        tags: vec!["mock".to_string(), "fixture".to_string()],
        text_content: "fixture text".to_string(),
        kind: "photo".to_string(),
    })
    .unwrap()
}

fn ids_json(ids: &[i64]) -> String {
    serde_json::to_string(&IdsPayload { ids: ids.to_vec() }).unwrap()
}

fn chat_content(content: String) -> MockResponse {
    MockResponse::Chat(MockChatResponse {
        choices: [MockChoice {
            message: MockMessage { content },
        }],
        usage: TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 1000,
        },
    })
}

fn write_json(stream: &mut TcpStream, status: u16, value: &impl Serialize) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = serde_json::to_string(value).unwrap();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}
