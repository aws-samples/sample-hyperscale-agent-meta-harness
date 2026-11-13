//! A scripted HTTP/1.1 server on 127.0.0.1 for the client tests: it records every request and answers what the handler says, including
//! slow, lying and endless answers. No test reaches the network.

#![allow(dead_code, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug, Default)]
pub struct Recorded {
    pub method: String,
    pub target: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Clone, Debug)]
pub enum Reply {
    /// A complete answer; `Content-Length` is added unless the headers carry one.
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Waits, then answers.
    Delayed(Duration, Box<Reply>),
    /// Headers with `Transfer-Encoding: chunked`, then chunks of `chunk` bytes until the client goes away.
    Endless { status: u16, chunk: usize },
    /// Closes the connection without an answer.
    Close,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Reply {
        Reply::Respond {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn bytes(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        Reply::Respond {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }
}

pub struct FakeServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let handler = Arc::new(handler);
        let log = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let _ = serve_one(stream, handler, log).await;
                });
            }
        });
        FakeServer {
            addr,
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An address nothing listens on.
pub async fn closed_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

async fn serve_one(
    mut stream: TcpStream,
    handler: Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>,
    log: Arc<Mutex<Vec<Recorded>>>,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let want: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < want {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let rec = Recorded {
        method,
        target,
        headers,
        body,
    };
    log.lock().unwrap().push(rec.clone());
    let mut reply = handler(&rec);
    while let Reply::Delayed(d, inner) = reply {
        tokio::time::sleep(d).await;
        reply = *inner;
    }
    match reply {
        Reply::Respond {
            status,
            headers,
            body,
        } => {
            let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
            let has_len = headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("content-length"));
            for (n, v) in &headers {
                out.push_str(&format!("{n}: {v}\r\n"));
            }
            if !has_len {
                out.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            out.push_str("\r\n");
            stream.write_all(out.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await
        }
        Reply::Endless { status, chunk } => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n"
            );
            stream.write_all(head.as_bytes()).await?;
            let data = vec![b'x'; chunk];
            loop {
                stream
                    .write_all(format!("{:x}\r\n", data.len()).as_bytes())
                    .await?;
                stream.write_all(&data).await?;
                stream.write_all(b"\r\n").await?;
            }
        }
        Reply::Close | Reply::Delayed(..) => Ok(()),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
