//! A scripted Exa API on loopback plain http for the engine's tests of the web tools: each
//! connection is answered by the next [`Answer`] of the script and every request is recorded.
//! A test crate uses it with `mod fake_gateway; mod fake_exa;`.

#![allow(dead_code, reason = "each test crate uses its own part of the fake")]

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::fake_gateway::{Request, read_request};

/// How the fake answers one connection.
pub(crate) enum Answer {
    Json(u16, Value),
    Raw(Vec<u8>),
    /// Reads the request and never answers.
    Hang,
    /// A `200` announcing and sending one byte more than the helper's reply cap.
    Oversized,
}

/// A scripted Exa API on a loopback port; every request is recorded.
pub(crate) struct FakeExa {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl FakeExa {
    pub(crate) fn start(script: Vec<Answer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let accept = {
            let (requests, stopping) = (Arc::clone(&requests), Arc::clone(&stopping));
            thread::spawn(move || {
                let mut script = script.into_iter();
                let mut workers = Vec::new();
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let (Ok(stream), Some(answer)) = (stream, script.next()) else {
                        continue;
                    };
                    let (requests, stopping) = (Arc::clone(&requests), Arc::clone(&stopping));
                    workers.push(thread::spawn(move || {
                        serve(stream, answer, &requests, &stopping);
                    }));
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })
        };
        Self {
            port,
            requests,
            stopping,
            accept: Some(accept),
        }
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeExa {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(("127.0.0.1", self.port)));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn serve(mut stream: TcpStream, answer: Answer, requests: &Mutex<Vec<Request>>, stop: &AtomicBool) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    requests.lock().unwrap().push(request);
    let head = |status: u16, length: usize| {
        format!(
            "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
    };
    match answer {
        Answer::Json(status, body) => {
            let body = body.to_string();
            let _ = stream.write_all(format!("{}{body}", head(status, body.len())).as_bytes());
        }
        Answer::Raw(bytes) => {
            let _ = stream.write_all(&bytes);
        }
        Answer::Hang => {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Answer::Oversized => {
            let length = 16 * 1024 * 1024 + 1;
            let _ = stream.write_all(head(200, length).as_bytes());
            let chunk = vec![b' '; 64 * 1024];
            let mut sent = 0;
            while sent < length {
                let n = chunk.len().min(length - sent);
                if stream.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
        }
    }
}

/// An Exa `/contents` answer with one result per `(url, text)`.
pub(crate) fn pages(results: &[(&str, &str)]) -> Value {
    let results: Vec<Value> = results
        .iter()
        .map(|(url, text)| json!({"url": url, "title": format!("T {url}"), "text": text}))
        .collect();
    json!({"requestId": "r1", "results": results, "statuses": []})
}
