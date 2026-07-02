use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

pub struct MockServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// A content-based find router: maps a marker string (expected to appear in
/// the request body) to a canned `ids` response. The first matching marker
/// wins. If no marker matches, falls through to the `find_ids` queue or
/// `[0]`.
pub type FindRouter = Vec<(String, Vec<i64>)>;

#[derive(Default)]
struct MockState {
    captions: Mutex<VecDeque<String>>,
    find_ids: Mutex<VecDeque<Vec<i64>>>,
    /// Optional content-based router for find responses. When set, each "hits"
    /// request is matched against the router entries: the first entry whose
    /// marker string appears in the request body determines the response.
    find_router: Mutex<FindRouter>,
    /// Optional canned raw content for find responses (for bad-JSON tests).
    find_raw: Mutex<VecDeque<String>>,
}

impl MockServer {
    pub fn start() -> Self {
        Self::with(vec![], vec![])
    }

    pub fn with(captions: Vec<&str>, find_ids: Vec<Vec<i64>>) -> Self {
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
        let state = Arc::new(MockState {
            captions: Mutex::new(captions.into_iter().map(str::to_string).collect()),
            find_ids: Mutex::new(find_ids),
            find_router: Mutex::new(find_router),
            find_raw: Mutex::new(find_raw),
        });
        let thread_requests = Arc::clone(&requests);
        let thread_running = Arc::clone(&running);
        let thread_state = Arc::clone(&state);
        let handle = thread::spawn(move || {
            while thread_running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        handle_client(stream, &thread_state);
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
            running,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_client(mut stream: TcpStream, state: &MockState) {
    let Ok((path, body, headers)) = read_request(&mut stream) else {
        return;
    };
    if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("authorization: bearer bad-cerebras"))
    {
        write_json(&mut stream, 401, &json!({"error":"bad key"}));
        return;
    }
    let response = match path.as_str() {
        "/chat/completions" => chat_response(&body, state),
        _ => (404, json!({"error":"not found"})),
    };
    write_json(&mut stream, response.0, &response.1);
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
        .unwrap_or(buf.len());
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();
    Ok((path, body, headers))
}

fn chat_response(body: &str, state: &MockState) -> (u16, Value) {
    let body: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
    let schema_name = body
        .pointer("/response_format/json_schema/name")
        .and_then(Value::as_str);
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
            // F11: content-based routing. Check the raw request body for marker
            // strings. The first matching marker determines the response ids.
            let raw_body = serde_json::to_string(&body).unwrap_or_default();
            let router = state.find_router.lock().unwrap();
            for (marker, ids) in router.iter() {
                if raw_body.contains(marker) {
                    return (200, chat_content(json!({"ids": ids}).to_string()));
                }
            }
            drop(router);

            // Check for raw canned content (for parse-failure tests).
            if let Some(raw) = state.find_raw.lock().unwrap().pop_front() {
                return (200, chat_content(raw));
            }

            // Default: pop from the find_ids queue.
            let ids = state
                .find_ids
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| vec![0]);
            (200, chat_content(json!({"ids": ids}).to_string()))
        }
        _ => (200, chat_content("ok".to_string())),
    }
}

fn caption_json(description: &str) -> String {
    json!({
        "description": description,
        "filename": "mock-image",
        "tags": ["mock", "fixture"],
        "text_content": "fixture text",
        "kind": "photo"
    })
    .to_string()
}

fn chat_content(content: String) -> Value {
    json!({
        "choices": [{"message": {"content": content}}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 1000}
    })
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = serde_json::to_string(value).unwrap();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}
