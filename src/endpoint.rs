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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        self.models_with_cancel(session_key, || false)
    }

    pub fn models_with_cancel(
        &self,
        session_key: Option<&str>,
        cancelled: impl Fn() -> bool,
    ) -> Result<Vec<crate::catalog::Model>> {
        if cancelled() {
            bail!("Model catalog request cancelled before dispatch");
        }
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
            let fetch = async {
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
            if response.content_length().is_some_and(|length| length > 1024 * 1024) {
                bail!("Endpoint model catalog exceeds 1 MiB");
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    bail!("Endpoint model catalog exceeds 1 MiB");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>(bytes)
            };
            tokio::pin!(fetch);
            loop {
                tokio::select! {
                    result = &mut fetch => break result,
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {
                        if cancelled() { bail!("Model catalog request cancelled during endpoint discovery"); }
                    }
                }
            }
        })?;
        if cancelled() {
            bail!("Model catalog request cancelled before accepting metadata");
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).context("Endpoint model catalog was not JSON")?;
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            if let Some(entries) = value.get_mut("data").and_then(Value::as_array_mut) {
                for entry in entries {
                    if entry
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.contains(&key))
                    {
                        entry["id"] = Value::Null;
                    }
                }
            }
        }
        crate::catalog::endpoint_value(&value)
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
        self.call_reasoned(model_id, prompt, timeout, cancelled, max_response, None)
    }

    pub fn call_reasoned(
        &self,
        model_id: &str,
        prompt: &str,
        timeout: Duration,
        cancelled: impl Fn() -> bool,
        max_response: u64,
        reasoning: Option<&str>,
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
        if let Some(effort) = reasoning {
            if !crate::catalog::valid_effort(effort) {
                bail!("invalid reasoning effort");
            }
            body["reasoning_effort"] = json!(effort);
        }
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
            let mut request = client.post(url).json(&model::request_body(&body));
            if let Some(key) = &key { request = request.bearer_auth(key); }
            let fetch = async {
                let mut response = request.send().await.map_err(|error| {
                    if error.is_connect() {
                        anyhow!(crate::direct::RecoverableFailure {
                            reason: crate::routing::Reason::Outage,
                            status: None,
                        })
                    } else {
                        anyhow!("custom endpoint request failed")
                    }
                })?;
                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    if let Some(reason) = crate::direct::recoverable_status(status) {
                        return Err(anyhow!(crate::direct::RecoverableFailure {
                            reason,
                            status: Some(status),
                        }));
                    }
                    bail!("custom endpoint returned HTTP {status}; response body omitted to protect credentials");
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
        let reported_usage = envelope.get("usage").and_then(|usage| {
            Some(Usage {
                input_tokens: usage.get("prompt_tokens")?.as_u64()?,
                output_tokens: usage.get("completion_tokens")?.as_u64()?,
                cached_input_tokens: usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                source: "provider".into(),
                cached_input_reported: usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).is_some(),
            })
        });
        let text = envelope
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(crate::direct::RejectedResponse {
                    usage: reported_usage.clone(),
                    shape: json!({"assistant_text":false}),
                })
            })?;
        let raw = key
            .as_ref()
            .map_or_else(|| text.to_owned(), |key| text.replace(key, "[redacted]"));
        let action = model::parse_action(&raw).map_err(|_| {
            anyhow!(crate::direct::RejectedResponse {
                usage: reported_usage.clone(),
                shape: crate::direct::response_shape(&raw),
            })
        })?;
        let usage = reported_usage.unwrap_or_else(|| Usage {
            input_tokens: (prompt.chars().count() as u64).div_ceil(4),
            output_tokens: (raw.chars().count() as u64).div_ceil(4),
            cached_input_tokens: 0,
            source: "estimated".into(),
            cached_input_reported: false,
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
    fn rejected_actions_and_missing_assistant_text_preserve_only_reported_usage() -> Result<()> {
        for content in [Value::Null, json!("private-invalid-action")] {
            for reported in [true, false] {
                let mut body = json!({"choices":[{"message":{"content":content}}]});
                if reported {
                    body["usage"] = json!({"prompt_tokens":20,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":10}});
                }
                let (endpoint, worker) = server("200 OK", "", &body.to_string(), Duration::ZERO)?;
                let error = endpoint
                    .call_bounded(
                        "fixture-model",
                        "request",
                        Duration::from_secs(2),
                        || false,
                        65536,
                    )
                    .err()
                    .context("Malformed action must be rejected")?;
                worker.join().unwrap();
                let rejected = error
                    .downcast_ref::<crate::direct::RejectedResponse>()
                    .context("Rejected receipt missing")?;
                assert!(!error.to_string().contains("private-invalid-action"));
                assert!(
                    !rejected
                        .shape
                        .to_string()
                        .contains("private-invalid-action")
                );
                if reported {
                    let usage = rejected.usage.as_ref().context("Reported usage missing")?;
                    assert_eq!(usage.input_tokens, 20);
                    assert_eq!(usage.output_tokens, 3);
                    assert_eq!(usage.cached_input_tokens, 10);
                    assert_eq!(usage.source, "provider");
                } else {
                    assert!(rejected.usage.is_none());
                }
            }
        }
        Ok(())
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
    fn only_retryable_endpoint_failures_allow_provider_migration() -> Result<()> {
        use crate::routing::Reason;

        for (status, expected) in [
            ("429 Too Many Requests", Some(Reason::UsageLimit)),
            ("410 Gone", Some(Reason::ModelRemoved)),
            ("503 Service Unavailable", Some(Reason::Outage)),
            ("400 Bad Request", None),
            ("401 Unauthorized", None),
        ] {
            let (endpoint, thread) = server(status, "", "private-error-body", Duration::ZERO)?;
            let error = endpoint
                .call("model", "Return JSON", Duration::from_secs(2), || false)
                .unwrap_err();
            thread.join().unwrap();
            assert_eq!(
                error
                    .downcast_ref::<crate::direct::RecoverableFailure>()
                    .map(|failure| failure.reason),
                expected
            );
            assert!(!format!("{error:#}").contains("private-error-body"));
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

    #[test]
    fn model_discovery_is_cancellable_and_cannot_echo_a_session_key_as_an_id() -> Result<()> {
        let (endpoint, thread) = server("200 OK", "", "{}", Duration::from_millis(500))?;
        let started = std::time::Instant::now();
        let error = endpoint
            .models_with_cancel(None, || started.elapsed() >= Duration::from_millis(100))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(450));
        assert!(error.to_string().contains("cancelled"));
        thread.join().unwrap();
        let (endpoint, thread) = server(
            "200 OK",
            "",
            "{\"data\":[{\"id\":\"fixture-session-key\"},{\"id\":\"public-model\"}]}",
            Duration::ZERO,
        )?;
        let models = endpoint.models(Some("fixture-session-key"))?;
        thread.join().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "public-model");
        assert!(
            endpoint
                .models_with_cancel(None, || true)
                .unwrap_err()
                .to_string()
                .contains("before dispatch")
        );
        Ok(())
    }
}
