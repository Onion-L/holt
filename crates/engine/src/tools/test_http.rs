//! The shared in-process loopback HTTP/1.1 server for tool tests — the
//! web tools never touch the real network in tests. Every connection is
//! answered from `respond`, which sees the request target (path plus
//! query) and body, and returns the raw response bytes; an empty return
//! parks the connection without answering, which keeps a request in
//! flight for timeout and cancellation tests.

#![allow(dead_code)] // each test module uses a different slice of the helpers

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(crate) struct Server {
    pub(crate) base: String,
    requests: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// The request heads received so far. Tests use this to pin the
    /// method, target, and headers a tool sends.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// The request bodies received so far, aligned with [`Server::requests`].
    pub(crate) fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn serve<F>(respond: F) -> Server
where
    F: Fn(&str, &[u8]) -> Vec<u8> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let respond = Arc::new(respond);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let recorded_bodies = Arc::clone(&bodies);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let respond = Arc::clone(&respond);
            let recorded = Arc::clone(&recorded);
            let recorded_bodies = Arc::clone(&recorded_bodies);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                let head_end = loop {
                    if let Some(end) = header_end(&request) {
                        // Hold the connection until the declared body has
                        // fully arrived, so POST bodies are recorded whole.
                        let head = String::from_utf8_lossy(&request[..end]);
                        if request.len() >= end + content_length(&head) {
                            break end;
                        }
                    }
                    let read = match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break header_end(&request).unwrap_or(request.len()),
                        Ok(read) => read,
                    };
                    request.extend_from_slice(&chunk[..read]);
                };
                let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                let body = request[head_end..].to_vec();
                recorded.lock().unwrap().push(head.clone());
                recorded_bodies.lock().unwrap().push(body.clone());
                let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let response = respond(&target, &body);
                if response.is_empty() {
                    std::future::pending::<()>().await;
                }
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Server {
        base,
        requests,
        bodies,
        task,
    }
}

fn header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn content_length(head: &str) -> usize {
    head.split("\r\n")
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
        })
        .unwrap_or(0)
}

pub(crate) fn response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

pub(crate) fn redirect(location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_heads_and_bodies_of_post_requests() {
        let server = serve(|_, _| response("200 OK", "application/json", br#"{"ok":true}"#)).await;
        let client = reqwest::Client::new();
        let reply = client
            .post(format!("{}/search", server.base))
            .header("content-type", "application/json")
            .body(br#"{"q":"holt"}"#.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);

        assert!(server.requests()[0].starts_with("POST /search HTTP/1.1"));
        assert_eq!(server.bodies()[0], br#"{"q":"holt"}"#.to_vec());
    }
}
