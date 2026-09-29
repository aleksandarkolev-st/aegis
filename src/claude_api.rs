use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Client, Url};
use serde_json::{Value, json};

use crate::model::{self, Response, Usage};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const API_VERSION: &str = "2023-06-01";

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 32_768 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("Claude API key is unavailable or invalid");
    }
    Ok(())
}

fn catalog_page(value: &Value, key: &str) -> Result<(Vec<crate::catalog::Model>, Option<String>)> {
    let entries = value["data"]
        .as_array()
        .context("Claude API returned an invalid model catalog")?;
    if entries.len() > 1000 {
        bail!("Claude API model catalog page exceeds its bound");
    }
    let mut models = Vec::new();
    for entry in entries {
        let Some(id) = entry["id"]
            .as_str()
            .filter(|id| crate::catalog::valid_id(id) && !id.contains(key))
        else {
            continue;
        };
        let label = entry["display_name"]
            .as_str()
            .filter(|label| !label.contains(key))
            .map(crate::catalog::display_label)
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| id.to_owned());
        models.push(crate::catalog::Model {
            id: id.to_owned(),
            label,
        });
    }
    let next = match value["has_more"].as_bool() {
        Some(true) => Some(
            value["last_id"]
                .as_str()
                .filter(|id| crate::catalog::valid_id(id) && !id.contains(key))
                .context("Claude API model catalog omitted its next-page cursor")?
                .to_owned(),
        ),
        Some(false) => None,
        None => bail!("Claude API model catalog omitted pagination state"),
    };
    Ok((models, next))
}

pub fn models(key: &str, cancelled: impl Fn() -> bool) -> Result<Vec<crate::catalog::Model>> {
    models_at(Url::parse(MODELS_URL)?, key, cancelled)
}

fn effort_levels(value: &Value) -> Vec<String> {
    let effort = &value["capabilities"]["effort"];
    if effort["supported"] != true {
        return Vec::new();
    }
    ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .filter(|level| effort[level]["supported"] == true)
        .map(str::to_owned)
        .collect()
}

pub fn efforts(model: &str, key: &str, cancelled: impl Fn() -> bool) -> Result<Vec<String>> {
    efforts_at(Url::parse(MODELS_URL)?, model, key, cancelled)
}

fn efforts_at(
    mut url: Url,
    model: &str,
    key: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<String>> {
    validate_key(key)?;
    if !crate::catalog::valid_id(model) || model.contains('/') || model.contains('\\') {
        bail!("Claude API needs a valid model ID for effort discovery");
    }
    if cancelled() {
        bail!("Claude API effort discovery cancelled before dispatch");
    }
    url.path_segments_mut()
        .map_err(|_| anyhow!("Claude API model URL is invalid"))?
        .push(model);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let fetch = async {
            let mut response = client
                .get(url)
                .bearer_auth(key)
                .header("anthropic-version", API_VERSION)
                .send()
                .await
                .map_err(|_| anyhow!("Claude API effort discovery unavailable"))?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                bail!("Claude API effort discovery returned HTTP {status}; remote body omitted");
            }
            if response
                .content_length()
                .is_some_and(|length| length > 1024 * 1024)
            {
                bail!("Claude API model metadata exceeds 1 MiB");
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow!("Claude API effort discovery interrupted"))?
            {
                if bytes.len() + chunk.len() > 1024 * 1024 {
                    bail!("Claude API model metadata exceeds 1 MiB");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>(bytes)
        };
        tokio::pin!(fetch);
        let bytes = loop {
            tokio::select! {
                result = &mut fetch => break result,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancelled() { bail!("Claude API effort discovery cancelled"); }
                }
            }
        }?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("Claude API model metadata was not JSON"))?;
        if !value["id"]
            .as_str()
            .is_some_and(|id| crate::catalog::valid_id(id) && !id.contains(key))
        {
            bail!("Claude API model metadata omitted a valid model ID");
        }
        Ok(effort_levels(&value))
    })
}

fn models_at(
    url: Url,
    key: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<crate::catalog::Model>> {
    validate_key(key)?;
    if cancelled() {
        bail!("Claude API model discovery cancelled before dispatch");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        let mut models = Vec::new();
        for _ in 0..5 {
            if cancelled() {
                bail!("Claude API model discovery cancelled");
            }
            let mut page_url = url.clone();
            {
                let mut query = page_url.query_pairs_mut();
                query.append_pair("limit", "1000");
                if let Some(cursor) = cursor.as_deref() {
                    query.append_pair("after_id", cursor);
                }
            }
            let fetch = async {
                let mut response = client
                    .get(page_url)
                    .bearer_auth(key)
                    .header("anthropic-version", API_VERSION)
                    .send()
                    .await
                    .map_err(|_| anyhow!("Claude API model discovery unavailable"))?;
                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    bail!("Claude API model discovery returned HTTP {status}; remote body omitted");
                }
                if response
                    .content_length()
                    .is_some_and(|length| length > 1024 * 1024)
                {
                    bail!("Claude API model catalog exceeds 1 MiB per page");
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| anyhow!("Claude API model discovery interrupted"))?
                {
                    if bytes.len() + chunk.len() > 1024 * 1024 {
                        bail!("Claude API model catalog exceeds 1 MiB per page");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok::<_, anyhow::Error>(bytes)
            };
            tokio::pin!(fetch);
            let bytes = loop {
                tokio::select! {
                    result = &mut fetch => break result,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if cancelled() { bail!("Claude API model discovery cancelled"); }
                    }
                }
            }?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow!("Claude API model catalog was not JSON"))?;
            let (page, next) = catalog_page(&value, key)?;
            for model in page {
                if seen.insert(model.id.clone()) {
                    models.push(model);
                }
            }
            if models.len() > 4096 {
                bail!("Claude API model catalog exceeds its total bound");
            }
            match next {
                Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
                Some(_) => bail!("Claude API model catalog repeated its page cursor"),
                None => return Ok(models),
            }
        }
        bail!("Claude API model catalog exceeded five pages")
    })
}

fn body(model: &str, prompt: &str, output_tokens: u64, effort: Option<&str>) -> Result<Value> {
    if !crate::catalog::valid_id(model) || !(1..=300_000).contains(&output_tokens) {
        bail!("Claude API needs a valid model and bounded output-token limit");
    }
    let mut schema = model::schema()?;
    schema["additionalProperties"] = json!(false);
    let mut request = json!({
        "model": model,
        "max_tokens": output_tokens,
        "messages": [{"role":"user","content":prompt}],
        "output_config": {"format":{"type":"json_schema","schema":schema}},
    });
    if let Some(effort) = effort {
        if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            bail!("Claude API effort must be a supported level");
        }
        request["output_config"]["effort"] = json!(effort);
    }
    Ok(request)
}

fn usage(envelope: &Value) -> Result<Usage> {
    let receipt = &envelope["usage"];
    let uncached = receipt["input_tokens"]
        .as_u64()
        .context("Claude API response omitted input usage")?;
    let output_tokens = receipt["output_tokens"]
        .as_u64()
        .context("Claude API response omitted output usage")?;
    let cached_input_tokens = receipt["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let cache_creation = receipt["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let input_tokens = uncached
        .checked_add(cached_input_tokens)
        .and_then(|count| count.checked_add(cache_creation))
        .context("Claude API usage overflow")?;
    Ok(Usage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        source: "provider".into(),
    })
}

fn decode(bytes: &[u8], key: &str) -> Result<Response> {
    let envelope: Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("Claude API response was not JSON"))?;
    let receipt = usage(&envelope)?;
    if envelope["stop_reason"] != "end_turn" {
        return Err(anyhow!(crate::direct::RejectedResponse {
            usage: Some(receipt),
            shape: json!({"stop_reason":envelope["stop_reason"],"complete":false}),
        }));
    }
    let blocks = envelope["content"].as_array();
    let mut text = None;
    let mut acceptable = blocks.is_some();
    for block in blocks.into_iter().flatten() {
        match block["type"].as_str() {
            Some("thinking" | "redacted_thinking") => {}
            Some("text") if text.is_none() => text = block["text"].as_str(),
            _ => acceptable = false,
        }
    }
    let Some(text) = text.filter(|_| acceptable) else {
        return Err(anyhow!(crate::direct::RejectedResponse {
            usage: Some(receipt),
            shape: json!({"content_blocks":blocks.map_or(0, Vec::len),"assistant_text":false}),
        }));
    };
    let raw = text.replace(key, "[redacted]");
    let action = model::parse_action(&raw).map_err(|_| {
        anyhow!(crate::direct::RejectedResponse {
            usage: Some(receipt.clone()),
            shape: crate::direct::response_shape(&raw),
        })
    })?;
    Ok(Response {
        action,
        raw,
        usage: Some(receipt),
    })
}

pub fn call(
    model: &str,
    prompt: &str,
    key: &str,
    output_tokens: u64,
    response_bytes: u64,
    effort: Option<&str>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
    call_at(
        Url::parse(MESSAGES_URL)?,
        model,
        prompt,
        key,
        output_tokens,
        response_bytes,
        effort,
        timeout,
        cancelled,
    )
}

fn call_at(
    url: Url,
    model: &str,
    prompt: &str,
    key: &str,
    output_tokens: u64,
    response_bytes: u64,
    effort: Option<&str>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
    validate_key(key)?;
    crate::budget::validate_response_bytes(response_bytes)?;
    let request = body(model, prompt, output_tokens, effort)?;
    if timeout.is_zero() || cancelled() {
        bail!("Claude API request interrupted before dispatch");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bytes = runtime.block_on(async {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(20)))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let fetch = async {
            let mut response = client
                .post(url)
                .bearer_auth(key)
                .header("anthropic-version", API_VERSION)
                .json(&request)
                .send()
                .await
                .map_err(|error| {
                    if error.is_connect() {
                        anyhow!(crate::direct::RecoverableFailure {
                            reason: crate::routing::Reason::Outage,
                            status: None,
                        })
                    } else {
                        anyhow!("Claude API connection failed")
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
                match status {
                    401 => bail!("Claude API key was rejected (HTTP 401); enter a current API key"),
                    403 => bail!("Claude API account does not permit this request (HTTP 403)"),
                    _ => bail!("Claude API request failed (HTTP {status}); remote body omitted"),
                }
            }
            if response
                .content_length()
                .is_some_and(|length| length > response_bytes)
            {
                bail!("Claude API response exceeds configured byte limit");
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow!("Claude API response interrupted"))?
            {
                if (bytes.len() + chunk.len()) as u64 > response_bytes {
                    bail!("Claude API response exceeds configured byte limit");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>(bytes)
        };
        tokio::pin!(fetch);
        loop {
            tokio::select! {
                result = &mut fetch => break result,
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancelled() { bail!("Claude API request interrupted"); }
                }
            }
        }
    })?;
    if cancelled() {
        bail!("Claude API request interrupted before accepting response");
    }
    decode(&bytes, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn native_request_uses_structured_messages_without_provider_tools() -> Result<()> {
        let request = body("account-model", "Refactor parser", 4096, Some("high"))?;
        assert_eq!(request["model"], "account-model");
        assert_eq!(request["max_tokens"], 4096);
        assert_eq!(request["messages"][0]["content"], "Refactor parser");
        assert_eq!(request["output_config"]["effort"], "high");
        assert_eq!(request["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            request["output_config"]["format"]["schema"]["additionalProperties"],
            false
        );
        assert!(request.get("tools").is_none());
        assert!(body("bad model", "work", 4096, None).is_err());
        assert!(body("model", "work", 0, None).is_err());
        assert!(body("model", "work", 4096, Some("minimal")).is_err());
        Ok(())
    }

    #[test]
    fn only_complete_text_actions_with_accounted_usage_are_accepted() -> Result<()> {
        let mut message = json!({
            "stop_reason":"end_turn",
            "content":[{"type":"text","text":"{\"kind\":\"finish\",\"summary\":\"Done\",\"evidence\":[]}"}],
            "usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7}
        });
        let response = decode(&serde_json::to_vec(&message)?, "private-key")?;
        assert!(matches!(response.action, model::Action::Finish { .. }));
        let receipt = response.usage.unwrap();
        assert_eq!(receipt.input_tokens, 35);
        assert_eq!(receipt.cached_input_tokens, 20);
        assert_eq!(receipt.output_tokens, 7);
        message["content"] = json!([
            {"type":"thinking","thinking":"private model reasoning"},
            {"type":"text","text":"{\"kind\":\"finish\",\"summary\":\"Done\",\"evidence\":[]}"}
        ]);
        assert!(matches!(
            decode(&serde_json::to_vec(&message)?, "private-key")?.action,
            model::Action::Finish { .. }
        ));
        message["stop_reason"] = json!("max_tokens");
        let error = decode(&serde_json::to_vec(&message)?, "private-key").unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::direct::RejectedResponse>()
                .is_some_and(|rejected| rejected.usage.is_some())
        );
        message["stop_reason"] = json!("end_turn");
        message["content"][0] = json!({"type":"tool_use","name":"Bash"});
        assert!(decode(&serde_json::to_vec(&message)?, "private-key").is_err());
        Ok(())
    }

    #[test]
    fn native_http_request_keeps_the_key_in_auth_header_only() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = Url::parse(&format!("http://{}/v1/messages", listener.local_addr()?))?;
        let server = thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut bytes = Vec::new();
            let (headers, body) = loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer)?;
                if count == 0 || bytes.len() + count > 65_536 {
                    bail!("fixture request was incomplete or too large");
                }
                bytes.extend_from_slice(&buffer[..count]);
                let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&bytes[..end])?;
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .context("fixture request omitted content length")?;
                if bytes.len() < end + 4 + length {
                    continue;
                }
                break (
                    headers.to_owned(),
                    bytes[end + 4..end + 4 + length].to_vec(),
                );
            };
            let lower = headers.to_ascii_lowercase();
            assert!(headers.starts_with("POST /v1/messages HTTP/1.1"));
            assert!(lower.contains("authorization: bearer private-fixture-key"));
            assert!(lower.contains("anthropic-version: 2023-06-01"));
            assert!(!String::from_utf8_lossy(&body).contains("private-fixture-key"));
            let request: Value = serde_json::from_slice(&body)?;
            assert_eq!(request["model"], "account-model");
            assert!(request.get("tools").is_none());
            let response = json!({
                "stop_reason":"end_turn",
                "content":[{"type":"text","text":"{\"kind\":\"finish\",\"summary\":\"Hello\",\"evidence\":[]}"}],
                "usage":{"input_tokens":9,"output_tokens":4}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )?;
            Ok(())
        });
        let response = call_at(
            url,
            "account-model",
            "hello",
            "private-fixture-key",
            4096,
            65_536,
            None,
            Duration::from_secs(5),
            || false,
        )?;
        server.join().map_err(|_| anyhow!("fixture panicked"))??;
        assert!(matches!(response.action, model::Action::Finish { .. }));
        assert_eq!(response.usage.unwrap().input_tokens, 9);
        Ok(())
    }

    #[test]
    fn model_catalog_uses_account_pages_without_a_fixed_model_list() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = Url::parse(&format!("http://{}/v1/models", listener.local_addr()?))?;
        let server = thread::spawn(move || -> Result<()> {
            for (index, page) in [
                json!({"data":[{"id":"account-a","display_name":"Account A"}],"has_more":true,"last_id":"account-a"}),
                json!({"data":[{"id":"account-a","display_name":"Duplicate"},{"id":"account-b","display_name":"Account B"}],"has_more":false,"last_id":"account-b"}),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut buffer = [0; 2048];
                    let count = stream.read(&mut buffer)?;
                    if count == 0 || request.len() + count > 8192 {
                        bail!("catalog fixture request was incomplete or too large");
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(request)?;
                assert!(request.starts_with("GET /v1/models?limit=1000"));
                assert_eq!(request.contains("after_id=account-a"), index == 1);
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer private-fixture-key"));
                let response = page.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
            Ok(())
        });
        let models = models_at(url, "private-fixture-key", || false)?;
        server.join().map_err(|_| anyhow!("fixture panicked"))??;
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "account-a");
        assert_eq!(models[1].label, "Account B");
        assert!(
            catalog_page(
                &json!({"data":[{"id":"private-fixture-key"}],"has_more":false}),
                "private-fixture-key"
            )?
            .0
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn effort_picker_uses_selected_model_capabilities_only() -> Result<()> {
        let metadata = json!({"id":"account-model","capabilities":{"effort":{
            "supported":true,"low":{"supported":true},"medium":{"supported":false},
            "high":{"supported":true},"xhigh":{"supported":true},"max":{"supported":false}
        }}});
        assert_eq!(effort_levels(&metadata), ["low", "high", "xhigh"]);
        assert!(
            effort_levels(
                &json!({"capabilities":{"effort":{"supported":false,"low":{"supported":true}}}})
            )
            .is_empty()
        );

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = Url::parse(&format!("http://{}/v1/models", listener.local_addr()?))?;
        let server = thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer)?;
                if count == 0 || request.len() + count > 8192 {
                    bail!("effort fixture request was incomplete or too large");
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(request)?;
            assert!(request.starts_with("GET /v1/models/account-model HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer private-fixture-key")
            );
            let response = metadata.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )?;
            Ok(())
        });
        assert_eq!(
            efforts_at(url.clone(), "account-model", "private-fixture-key", || {
                false
            })?,
            ["low", "high", "xhigh"]
        );
        server.join().map_err(|_| anyhow!("fixture panicked"))??;
        assert!(efforts_at(url, "bad/model", "private-fixture-key", || false).is_err());
        Ok(())
    }
}
