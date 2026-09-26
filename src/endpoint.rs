use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{self, Response, Usage};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Schema,
    Json,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub response_format: ResponseFormat,
    #[serde(default)]
    pub allow_insecure: bool,
}

impl Endpoint {
    pub fn models(&self, session_key: Option<&str>) -> Result<Vec<crate::catalog::Model>> {
        let mut url = self.url()?;
        let path = url.path().trim_end_matches("/chat/completions");
        url.set_path(&format!("{path}/models"));
        let key = session_key.map(str::to_owned).or_else(|| {
            self.api_key_env
                .as_ref()
                .and_then(|name| std::env::var(name).ok())
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let bytes = runtime.block_on(async {
            let client = Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let mut request = client.get(url);
            if let Some(key) = &key {
                request = request.bearer_auth(key);
            }
            let mut response = request
                .send()
                .await
                .context("Endpoint model discovery unavailable")?;
            if !response.status().is_success() {
                bail!(
                    "Endpoint model list returned HTTP {}; you can still enter a model ID",
                    response.status().as_u16()
                );
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    bail!("Endpoint model catalog exceeds 1 MiB");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>(bytes)
        })?;
        crate::catalog::endpoint_value(
            &serde_json::from_slice(&bytes).context("Endpoint model catalog was not JSON")?,
        )
    }

    pub fn url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.base_url).context("invalid custom endpoint URL")?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "endpoint URLs cannot contain credentials, query strings, or fragments; use an API-key environment reference"
            );
        }
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if url.scheme() != "https" && !(url.scheme() == "http" && (loopback || self.allow_insecure))
        {
            bail!(
                "custom endpoints require HTTPS or loopback HTTP; explicitly allow insecure HTTP for a trusted remote host"
            );
        }
        if let Some(name) = &self.api_key_env {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
                || name.as_bytes()[0].is_ascii_digit()
            {
                bail!("API key reference must be an environment variable name");
            }
        }
        let path = url.path().trim_end_matches('/');
        if !path.ends_with("/chat/completions") {
            let path = if path.is_empty() {
                "/v1/chat/completions".into()
            } else {
                format!("{path}/chat/completions")
            };
            url.set_path(&path);
        }
        Ok(url)
    }

    pub fn call(
        &self,
        model_id: &str,
        prompt: &str,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
    ) -> Result<Response> {
        self.call_bounded(
            model_id,
            prompt,
            timeout,
            cancelled,
            crate::budget::DEFAULT_MODEL_RESPONSE_BYTES,
        )
    }

    pub fn call_bounded(
        &self,
        model_id: &str,
        prompt: &str,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
        max_response: u64,
    ) -> Result<Response> {
        crate::budget::validate_response_bytes(max_response)?;
        let url = self.url()?;
        if model_id.trim().is_empty() {
            bail!("custom endpoints require a model ID");
        }
        if cancelled() {
            bail!("run cancelled before endpoint request");
        }
        let key = self
            .api_key_env
            .as_ref()
            .map(|name| {
                std::env::var(name)
                    .with_context(|| format!("API key environment variable {name} is not set"))
            })
            .transpose()?;
        if key.as_ref().is_some_and(|key| key.trim().is_empty()) {
            bail!("API key environment variable is empty");
        }
        let mut body = json!({"model":model_id, "messages":[{"role":"user","content":prompt}], "stream":false});
        match self.response_format {
            ResponseFormat::Schema => {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"runtime_action","strict":true,"schema":model::schema()?}})
            }
            ResponseFormat::Json => body["response_format"] = json!({"type":"json_object"}),
            ResponseFormat::None => {}
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let bytes = runtime.block_on(async {
            let client = Client::builder().timeout(timeout).connect_timeout(timeout.min(Duration::from_secs(20)))
                .redirect(reqwest::redirect::Policy::none()).build()?;
            let mut request = client.post(url).json(&body);
            if let Some(key) = &key { request = request.bearer_auth(key); }
            let fetch = async {
                let mut response = request.send().await.context("custom endpoint request failed")?;
                if !response.status().is_success() {
                    bail!("custom endpoint returned HTTP {}; response body omitted to protect credentials", response.status().as_u16());
                }
                if response.content_length().is_some_and(|bytes| bytes > max_response) {
                    bail!("custom endpoint response exceeds configured byte limit");
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.context("custom endpoint response interrupted")? {
                    if (bytes.len() + chunk.len()) as u64 > max_response { bail!("custom endpoint response exceeds configured byte limit"); }
                    bytes.extend_from_slice(&chunk);
                }
                Ok::<_, anyhow::Error>(bytes)
            };
            tokio::pin!(fetch);
            loop {
                tokio::select! {
                    response = &mut fetch => break response,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if cancelled() { bail!("run cancelled during endpoint request"); }
                    }
                }
            }
        })?;
        let envelope: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("custom endpoint response was not JSON"))?;
        let text = envelope
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .context("custom endpoint did not return assistant text")?;
        let raw = key
            .as_ref()
            .map_or_else(|| text.to_owned(), |key| text.replace(key, "[redacted]"));
        let action = model::parse_action(&raw)
            .map_err(|_| anyhow!("custom endpoint returned invalid action JSON"))?;
        let usage = envelope
            .get("usage")
            .and_then(|usage| {
                Some(Usage {
                    input_tokens: usage.get("prompt_tokens")?.as_u64()?,
                    output_tokens: usage.get("completion_tokens")?.as_u64()?,
                    cached_input_tokens: usage
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    source: "provider".into(),
                })
            })
            .unwrap_or_else(|| Usage {
                input_tokens: (prompt.chars().count() as u64).div_ceil(4),
                output_tokens: (raw.chars().count() as u64).div_ceil(4),
                cached_input_tokens: 0,
                source: "estimated".into(),
            });
        Ok(Response {
            action,
            raw,
            usage: Some(usage),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str) -> Endpoint {
        Endpoint {
            base_url: url.into(),
            api_key_env: None,
            response_format: ResponseFormat::Schema,
            allow_insecure: false,
        }
    }

    #[test]
    fn validates_and_normalizes_endpoint_paths_without_accepting_secrets() -> Result<()> {
        for (base, path) in [
            ("http://127.0.0.1:1234", "/v1/chat/completions"),
            ("https://example.test/api/v1/", "/api/v1/chat/completions"),
            (
                "https://example.test/v1/chat/completions",
                "/v1/chat/completions",
            ),
        ] {
            assert_eq!(endpoint(base).url()?.path(), path);
        }
        for url in [
            "https://user:secret@example.test/v1",
            "https://example.test/v1?api_key=secret",
            "ftp://example.test",
            "http://example.test/v1",
        ] {
            assert!(endpoint(url).url().is_err());
        }
        Ok(())
    }

    fn server(
        status: &str,
        headers: &str,
        body: &str,
        delay: Duration,
    ) -> Result<(Endpoint, std::thread::JoinHandle<()>)> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let response = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            std::thread::sleep(delay);
            let _ = stream.write_all(response.as_bytes());
        });
        Ok((endpoint(&format!("http://{address}/v1")), thread))
    }

    #[test]
    fn rejects_redirects_and_omits_remote_error_bodies() -> Result<()> {
        for (status, headers) in [
            ("302 Found", "Location: https://example.test/elsewhere\r\n"),
            ("401 Unauthorized", ""),
        ] {
            let (endpoint, thread) = server(status, headers, "server-body-secret", Duration::ZERO)?;
            let error = endpoint
                .call("model", "Return JSON", Duration::from_secs(2), || false)
                .unwrap_err();
            thread.join().unwrap();
            assert!(error.to_string().contains("HTTP"));
            assert!(!format!("{error:#}").contains("server-body-secret"));
        }
        Ok(())
    }

    #[test]
    fn cancels_in_flight_http_requests() -> Result<()> {
        let (endpoint, thread) = server("200 OK", "", "{}", Duration::from_millis(500))?;
        let started = std::time::Instant::now();
        let error = endpoint
            .call("model", "Return JSON", Duration::from_secs(2), || {
                started.elapsed() >= Duration::from_millis(100)
            })
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(450));
        assert!(error.to_string().contains("cancelled"));
        thread.join().unwrap();
        Ok(())
    }
}
