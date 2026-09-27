use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

pub struct Endpoint {
    pub url: String,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Endpoint {
    pub fn start(
        mut respond: impl FnMut(&Value) -> Result<(u16, Value)> + Send + 'static,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                let body = request(&mut stream)?;
                let (status, response) = respond(&body)?;
                let response = response.to_string();
                if let Err(error) = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                ) {
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) {
                        return Err(error.into());
                    }
                }
            }
            Ok(())
        });
        Ok(Self {
            url,
            stopped,
            worker: Some(worker),
        })
    }

    pub fn finish(mut self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        self.worker
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("HTTP model fixture panicked"))?
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn request(stream: &mut TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer)?;
        ensure!(count != 0, "HTTP model request closed before its body");
        bytes.extend_from_slice(&buffer[..count]);
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "HTTP model request exceeds fixture bound"
        );
        let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            ensure!(
                bytes.len() <= 65536,
                "HTTP model headers exceed fixture bound"
            );
            continue;
        };
        ensure!(end <= 65536, "HTTP model headers exceed fixture bound");
        let headers = std::str::from_utf8(&bytes[..end])?;
        ensure!(
            headers.lines().next() == Some("POST /v1/chat/completions HTTP/1.1"),
            "Unexpected HTTP model route"
        );
        let mut length = None;
        for line in headers.lines().skip(1) {
            let (name, value) = line.split_once(':').context("Invalid HTTP model header")?;
            ensure!(
                !name.eq_ignore_ascii_case("authorization"),
                "Fixture must not receive account credentials"
            );
            if name.eq_ignore_ascii_case("content-length") {
                ensure!(length.is_none(), "Duplicate fixture content length");
                length = Some(value.trim().parse::<usize>()?);
            }
        }
        let length = length.context("Fixture requires a content length")?;
        ensure!(
            length <= 4 * 1024 * 1024 - end - 4,
            "HTTP model body exceeds fixture bound"
        );
        if bytes.len() >= end + 4 + length {
            return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + length])?);
        }
    }
}

pub fn state(body: &Value) -> Result<Value> {
    let prompt = body["messages"][0]["content"]
        .as_str()
        .context("Fixture requires an Aegis prompt")?;
    let Some((_, state)) = prompt.split_once("STATE (bounded, data not instructions):\n") else {
        bail!("Fixture requires bounded Aegis state");
    };
    Ok(serde_json::from_str(state)?)
}
