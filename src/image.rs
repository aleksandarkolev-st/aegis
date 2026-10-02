use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::storage::Store;

pub const MAX_IMAGES_PER_REQUEST: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 6 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_ARTIFACT_BYTES: usize = 12 * 1024 * 1024;
const MAX_UNMARKED_ARTIFACTS_TO_SCAN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputImage {
    pub(crate) mime_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) artifact_hash: String,
    pub(crate) content_index: usize,
}

impl InputImage {
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.mime_type,
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }

    pub(crate) fn context_metadata(&self) -> Value {
        json!({
            "artifact":self.artifact_hash,
            "content_index":self.content_index,
            "mime_type":self.mime_type,
            "bytes":self.bytes.len(),
        })
    }
}

/// Extracts visual inputs from the newest successful MCP image result. Only that
/// artifact is attached; it remains available until a newer image is returned.
pub(crate) fn latest_successful_mcp_images(store: &Store, run_id: &str) -> Result<Vec<InputImage>> {
    let events = store.events(run_id)?;
    let operations: HashMap<_, _> = store
        .operations(run_id)?
        .into_iter()
        .map(|operation| (operation.id.clone(), operation))
        .collect();

    let mut scanned = 0usize;
    for event in events
        .iter()
        .rev()
        .filter(|event| event.kind == "operation.succeeded")
    {
        let Some(operation_id) = event.payload["id"].as_str() else {
            continue;
        };
        let Some(operation) = operations.get(operation_id) else {
            continue;
        };
        let Some(hash) = event.payload["artifact"].as_str() else {
            continue;
        };
        if operation.state != "succeeded"
            || !operation.capability.starts_with("mcp.")
            || operation.artifact.as_deref() != Some(hash)
        {
            continue;
        }
        let recorded_size = event.payload["detail"]["bytes"].as_u64();
        let preview = &event.payload["detail"]["output_preview"];
        let preview_count = preview["image_count"]
            .as_u64()
            .or_else(|| preview["images"]["image_count"].as_u64());
        if preview_count == Some(0) {
            continue;
        }
        if preview_count.is_none() {
            scanned += 1;
            if scanned > MAX_UNMARKED_ARTIFACTS_TO_SCAN {
                break;
            }
        }
        if recorded_size.is_some_and(|size| size > MAX_IMAGE_ARTIFACT_BYTES as u64) {
            if preview_count.is_none_or(|count| count > 0) {
                bail!(
                    "MCP image result artifact exceeds the 12 MiB visual input limit; capture or return a smaller image"
                );
            }
            continue;
        }

        let bytes = store.artifact(hash)?;
        if bytes.len() > MAX_IMAGE_ARTIFACT_BYTES {
            if preview_count.is_none_or(|count| count > 0) {
                bail!(
                    "MCP image result artifact exceeds the 12 MiB visual input limit; capture or return a smaller image"
                );
            }
            continue;
        }
        let result: Value = serde_json::from_slice(&bytes)
            .context("successful MCP result artifact is not valid JSON")?;
        let images = extract_images(hash, &result)?;
        if !images.is_empty() {
            return Ok(images);
        }
    }
    Ok(Vec::new())
}

fn extract_images(artifact_hash: &str, result: &Value) -> Result<Vec<InputImage>> {
    extract_images_limited(
        artifact_hash,
        result,
        MAX_IMAGES_PER_REQUEST,
        MAX_IMAGE_BYTES,
        MAX_TOTAL_IMAGE_BYTES,
    )
}

fn extract_images_limited(
    artifact_hash: &str,
    result: &Value,
    max_images: usize,
    max_image_bytes: usize,
    max_total_bytes: usize,
) -> Result<Vec<InputImage>> {
    let Some(content) = result["content"].as_array() else {
        return Ok(Vec::new());
    };
    let mut images = Vec::new();
    let mut total_bytes = 0usize;
    for (content_index, block) in content.iter().enumerate() {
        if block["type"] != "image" {
            continue;
        }
        if images.len() == max_images {
            bail!(
                "MCP returned more than {max_images} images; ask it to return fewer image blocks"
            );
        }
        let mime_type = block["mimeType"]
            .as_str()
            .or_else(|| block["mime_type"].as_str())
            .context("MCP image block has no MIME type")?
            .to_ascii_lowercase();
        if !matches!(
            mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            bail!("MCP image MIME type {mime_type:?} is unsupported; use PNG, JPEG, GIF, or WebP");
        }
        let encoded = block["data"]
            .as_str()
            .context("MCP image block has no base64 data")?;
        if encoded.len() > base64_encoded_limit(max_image_bytes) {
            bail!("MCP image exceeds the {max_image_bytes} byte per-image visual input limit");
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("MCP image data is not valid standard base64")?;
        if decoded.is_empty() {
            bail!("MCP image data is empty");
        }
        if decoded.len() > max_image_bytes {
            bail!("MCP image exceeds the {max_image_bytes} byte per-image visual input limit");
        }
        if total_bytes.saturating_add(decoded.len()) > max_total_bytes {
            bail!(
                "MCP image set exceeds the {max_total_bytes} byte total visual input limit; return fewer or smaller images"
            );
        }
        validate_signature(&mime_type, &decoded)?;
        total_bytes += decoded.len();
        images.push(InputImage {
            mime_type,
            bytes: decoded,
            artifact_hash: artifact_hash.to_owned(),
            content_index,
        });
    }
    Ok(images)
}

fn base64_encoded_limit(bytes: usize) -> usize {
    bytes.saturating_add(2) / 3 * 4
}

fn validate_signature(mime_type: &str, bytes: &[u8]) -> Result<()> {
    let matches = match mime_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    };
    if !matches {
        bail!("MCP image bytes do not match declared MIME type {mime_type}");
    }
    Ok(())
}

pub(crate) fn image_preview(result: &Value) -> Option<Value> {
    let content = result["content"].as_array()?;
    let blocks: Vec<_> = content
        .iter()
        .filter(|block| block["type"] == "image")
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let mime_types: Vec<_> = blocks
        .iter()
        .map(|block| {
            let mime_type = block["mimeType"]
                .as_str()
                .or_else(|| block["mime_type"].as_str())
                .unwrap_or("unknown");
            let safe = mime_type.len() <= 64
                && mime_type.is_ascii()
                && mime_type
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-/".contains(&byte));
            if safe {
                mime_type.to_ascii_lowercase()
            } else {
                "unknown".to_owned()
            }
        })
        .collect();
    Some(json!({"image_count":blocks.len(),"mime_types":mime_types}))
}

/// Removes image payloads from JSON before text inspection or model-context use.
pub(crate) fn redact_mcp_images(value: &mut Value) -> bool {
    let Some(content) = value["content"].as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for block in content {
        if block["type"] == "image"
            && let Some(object) = block.as_object_mut()
        {
            changed |= object.remove("data").is_some();
        }
    }
    changed
}

pub(crate) fn redact_image_artifact_for_text(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    if !redact_mcp_images(&mut value) {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use serde_json::json;
    use std::path::Path;

    fn encoded_png(extra: &[u8]) -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(extra);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn image_blocks_are_validated_and_bounded() -> Result<()> {
        let png = encoded_png(b"fixture image data");
        let result = json!({"content":[{"type":"image","mimeType":"image/png","data":png}]});
        let images = extract_images(&"a".repeat(64), &result)?;
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type(), "image/png");
        assert_eq!(images[0].byte_len(), 8 + b"fixture image data".len());
        assert!(
            extract_images(
                "hash",
                &json!({"content":[{"type":"image","mimeType":"image/svg+xml","data":"PHN2Zy8+"}]})
            )
            .is_err()
        );
        assert!(
            extract_images(
                "hash",
                &json!({"content":[{"type":"image","mimeType":"image/png","data":"not base64!"}]})
            )
            .is_err()
        );
        assert!(
            extract_images(
                "hash",
                &json!({"content":[{"type":"image","mimeType":"image/jpeg","data":png}]})
            )
            .is_err()
        );
        let many = json!({"content":[
            {"type":"image","mimeType":"image/png","data":encoded_png(b"1")},
            {"type":"image","mimeType":"image/png","data":encoded_png(b"2")},
            {"type":"image","mimeType":"image/png","data":encoded_png(b"3")},
            {"type":"image","mimeType":"image/png","data":encoded_png(b"4")},
            {"type":"image","mimeType":"image/png","data":encoded_png(b"5")}
        ]});
        assert!(extract_images("hash", &many).is_err());
        let pair = json!({"content":[
            {"type":"image","mimeType":"image/png","data":encoded_png(b"1234")},
            {"type":"image","mimeType":"image/png","data":encoded_png(b"5678")}
        ]});
        assert!(extract_images_limited("hash", &pair, 4, 64, 20).is_err());
        assert!(extract_images_limited("hash", &pair, 4, 10, 64).is_err());
        Ok(())
    }

    #[test]
    fn latest_successful_image_reuses_its_artifact_until_replaced() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "inspect screenshot",
            Path::new(directory.path()),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"fixture"}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation =
            store.begin_operation(&run.id, "mcp.fixture.screenshot", json!({}), false)?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        let result = json!({"content":[{"type":"image","mimeType":"image/png","data":encoded_png(b"visible scene")}]});
        let result_bytes = serde_json::to_vec(&result)?;
        let hash = store.put_artifact(&result_bytes)?;
        store.operation_state(
            &operation,
            "succeeded",
            Some(&hash),
            json!({"capability":"mcp.fixture.screenshot","bytes":result_bytes.len(),"output_preview":{"image_count":1}}),
        )?;
        let images = latest_successful_mcp_images(&store, &run.id)?;
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].artifact_hash, hash);
        assert_eq!(images[0].content_index, 0);

        store.event(&run.id, "model.response", json!({"fixture":true}))?;
        let text_operation =
            store.begin_operation(&run.id, "mcp.fixture.status", json!({}), true)?;
        store.operation_state(&text_operation, "dispatched", None, json!({}))?;
        crate::storage::claim_test_operation(&mut store, &text_operation)?;
        crate::kernel::commit_result(
            &mut store,
            &text_operation,
            json!({"content":[{"type":"text","text":"still open"}]}),
            5,
        )?;
        assert_eq!(
            latest_successful_mcp_images(&store, &run.id)?[0].artifact_hash,
            hash
        );

        let newer = store.begin_operation(&run.id, "mcp.fixture.screenshot", json!({}), false)?;
        store.operation_state(&newer, "dispatched", None, json!({}))?;
        crate::storage::claim_test_operation(&mut store, &newer)?;
        let newer_result = json!({"content":[{"type":"image","mimeType":"image/png","data":encoded_png(b"new scene")}]});
        let newer_bytes = serde_json::to_vec(&newer_result)?;
        let newer_hash = store.put_artifact(&newer_bytes)?;
        store.operation_state(
            &newer,
            "succeeded",
            Some(&newer_hash),
            json!({"capability":"mcp.fixture.screenshot","bytes":newer_bytes.len(),"output_preview":{"image_count":1}}),
        )?;
        assert_eq!(
            latest_successful_mcp_images(&store, &run.id)?[0].artifact_hash,
            newer_hash
        );
        Ok(())
    }

    #[test]
    fn image_payload_redaction_keeps_only_safe_metadata() -> Result<()> {
        let marker = encoded_png(b"MCP_PRIVATE_IMAGE_DATA");
        let mut value = json!({"content":[{"type":"image","mimeType":"image/png","data":marker}]});
        assert!(redact_mcp_images(&mut value));
        assert_eq!(value["content"][0]["mimeType"], "image/png");
        assert!(value["content"][0].get("data").is_none());
        assert!(!value.to_string().contains("MCP_PRIVATE_IMAGE_DATA"));
        Ok(())
    }
}
