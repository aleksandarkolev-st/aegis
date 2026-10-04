use anyhow::{Context, Result, bail};
use base64::Engine;
use rusqlite::{Connection, Transaction, params};
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

pub(crate) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS image_sources (
        run_id TEXT NOT NULL REFERENCES runs(id), seq INTEGER NOT NULL,
        operation_id TEXT NOT NULL, artifact TEXT NOT NULL, detail TEXT NOT NULL,
        PRIMARY KEY(run_id,seq)
    );
    CREATE TABLE IF NOT EXISTS image_migrations(run_id TEXT PRIMARY KEY REFERENCES runs(id));
    CREATE TABLE IF NOT EXISTS image_rejections(run_id TEXT NOT NULL REFERENCES runs(id), artifact TEXT NOT NULL, error TEXT NOT NULL, PRIMARY KEY(run_id,artifact));")?;
    Ok(())
}

pub(crate) fn track(
    transaction: &Transaction<'_>,
    run_id: &str,
    seq: i64,
    kind: &str,
    payload: &Value,
) -> Result<()> {
    if kind == "run.created" {
        transaction.execute(
            "INSERT OR IGNORE INTO image_migrations(run_id) VALUES (?1)",
            [run_id],
        )?;
    }
    if kind != "operation.succeeded" {
        return Ok(());
    }
    let Some(id) = payload["id"].as_str() else {
        return Ok(());
    };
    let Some(hash) = payload["artifact"].as_str() else {
        return Ok(());
    };
    let detail = &payload["detail"];
    let count = image_count(detail);
    if count == Some(0) {
        return Ok(());
    }
    transaction.execute("INSERT OR IGNORE INTO image_sources(run_id,seq,operation_id,artifact,detail)
        SELECT ?1,?2,id,?4,?5 FROM operations WHERE id=?3 AND run_id=?1 AND capability LIKE 'mcp.%'",
        params![run_id,seq,id,hash,detail.to_string()])?;
    Ok(())
}

fn image_count(detail: &Value) -> Option<u64> {
    detail["output_preview"]["image_count"]
        .as_u64()
        .or_else(|| detail["output_preview"]["images"]["image_count"].as_u64())
}

pub(crate) fn restore(store: &Store) -> Result<()> {
    let ids = store.connection.prepare("SELECT id FROM runs WHERE NOT EXISTS(SELECT 1 FROM image_migrations WHERE run_id=runs.id)")?
        .query_map([], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        loop {
            let events = store.events(&id)?;
            let seq = events.last().map_or(0, |event| event.seq);
            let transaction = rusqlite::Transaction::new_unchecked(
                &store.connection,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let current: i64 = transaction.query_row(
                "SELECT last_seq FROM run_projection WHERE run_id=?1",
                [&id],
                |row| row.get(0),
            )?;
            if seq != current {
                continue;
            }
            if transaction.execute(
                "INSERT OR IGNORE INTO image_migrations(run_id) VALUES (?1)",
                [&id],
            )? == 1
            {
                for event in events {
                    track(&transaction, &id, event.seq, &event.kind, &event.payload)?;
                }
            }
            transaction.commit()?;
            break;
        }
    }
    Ok(())
}

/// Read a bounded indexed window rather than replaying the event archive.
pub(crate) fn latest_successful_mcp_images(
    store: &mut Store,
    run_id: &str,
) -> Result<Vec<InputImage>> {
    let sources = store.connection.prepare("SELECT source.artifact,source.detail FROM image_sources AS source
        JOIN operations AS operation ON operation.id=source.operation_id AND operation.run_id=source.run_id
        WHERE source.run_id=?1 AND operation.state='succeeded' AND operation.artifact=source.artifact
        ORDER BY source.seq DESC LIMIT ?2")?
        .query_map(params![run_id, MAX_UNMARKED_ARTIFACTS_TO_SCAN + 1], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut scanned = 0usize;
    for (hash, detail) in sources {
        let detail: Value = serde_json::from_str(&detail)?;
        if image_count(&detail).is_none() {
            scanned += 1;
            if scanned > MAX_UNMARKED_ARTIFACTS_TO_SCAN {
                break;
            }
        }
        let rejected: bool = store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM image_rejections WHERE run_id=?1 AND artifact=?2)",
            params![run_id, hash],
            |row| row.get(0),
        )?;
        if rejected {
            return Ok(Vec::new());
        }
        // Artifact integrity errors still stop recovery. Only visual decoding
        // failures in an intact receipt are quarantined for model recovery.
        let bytes = store.artifact(&hash)?;
        let decoded = (|| -> Result<Vec<InputImage>> {
            if bytes.len() > MAX_IMAGE_ARTIFACT_BYTES
                || detail["bytes"]
                    .as_u64()
                    .is_some_and(|size| size > MAX_IMAGE_ARTIFACT_BYTES as u64)
            {
                bail!(
                    "MCP image result artifact exceeds the 12 MiB visual input limit; capture or return a smaller image"
                );
            }
            let result: Value = serde_json::from_slice(&bytes)
                .context("successful MCP result artifact is not valid JSON")?;
            let images = extract_images(&hash, &result)?;
            if image_count(&detail).is_some_and(|count| count > 0) && images.is_empty() {
                bail!("MCP image result metadata claims images but its receipt contains none");
            }
            Ok(images)
        })();
        match decoded {
            Ok(images) if !images.is_empty() => return Ok(images),
            Ok(_) => {
                store.connection.execute(
                    "DELETE FROM image_sources WHERE run_id=?1 AND artifact=?2",
                    params![run_id, hash],
                )?;
            }
            Err(error) => {
                let reason: String = crate::text::clean(&format!("{error:#}"))
                    .chars()
                    .take(1024)
                    .collect();
                let transaction = store.connection.unchecked_transaction()?;
                transaction.execute("INSERT OR IGNORE INTO image_rejections(run_id,artifact,error) VALUES (?1,?2,?3)", params![run_id,hash,reason])?;
                crate::storage::append_event(
                    &transaction,
                    run_id,
                    "visual_input.rejected",
                    json!({"artifact":hash,"error":reason,"policy":"Image omitted; capture or request a replacement before relying on visual contents"}),
                )?;
                transaction.commit()?;
                return Ok(Vec::new());
            }
        }
    }
    Ok(Vec::new())
}

pub(crate) fn add_context(store: &Store, run_id: &str, context: &mut Value) -> Result<()> {
    use rusqlite::OptionalExtension;
    let rejected: Option<(String,String)> = store.connection.query_row(
        "SELECT source.artifact,rejection.error FROM image_sources AS source
        JOIN image_rejections AS rejection ON rejection.run_id=source.run_id AND rejection.artifact=source.artifact
        WHERE source.run_id=?1 AND source.seq=(SELECT MAX(seq) FROM image_sources WHERE run_id=?1)",
        [run_id], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    if let Some((artifact, error)) = rejected {
        context["visual_input_error"] = json!({"artifact":artifact,"error":error,"policy":"The newest image could not be decoded and is omitted. No older screenshot is substituted. Capture a new screenshot with a granted tool or ask the user for a replacement; continue independent work. Do not claim to have seen this image."});
    }
    Ok(())
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
    fn malformed_image_is_quarantined_without_reusing_older_screenshots() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Inspect",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for (index, data) in [
            encoded_png(b"old scene"),
            "not base64!".into(),
            encoded_png(b"replacement scene"),
        ]
        .into_iter()
        .enumerate()
        {
            let operation = store.begin_operation(
                &run.id,
                "mcp.fixture.capture",
                json!({"capture":index}),
                true,
            )?;
            crate::storage::claim_test_operation(&mut store, &operation)?;
            crate::kernel::commit_result(
                &mut store,
                &operation,
                json!({"content":[{"type":"image","mimeType":"image/png","data":data}]}),
                1,
            )?;
            let images = latest_successful_mcp_images(&mut store, &run.id)?;
            let mut context = json!({});
            add_context(&store, &run.id, &mut context)?;
            if index == 1 {
                assert!(images.is_empty());
                assert!(
                    context["visual_input_error"]["error"]
                        .as_str()
                        .unwrap()
                        .contains("base64")
                );
                for _ in 0..1100 {
                    store.event(&run.id, "telemetry", json!({}))?;
                }
                store.maintain_history(&run.id)?;
                drop(store);
                store = Store::open(&root)?;
                assert!(latest_successful_mcp_images(&mut store, &run.id)?.is_empty());
                assert_eq!(store.event_count(&run.id, "visual_input.rejected")?, 1);
                let mut restored = json!({});
                add_context(&store, &run.id, &mut restored)?;
                assert_eq!(context, restored);
            } else {
                assert_eq!(images.len(), 1);
                assert_eq!(
                    images[0].artifact_hash,
                    store.operation(&operation.id)?.artifact.unwrap()
                );
                assert!(context.get("visual_input_error").is_none());
            }
        }
        Ok(())
    }

    #[test]
    fn image_lookup_is_independent_of_twelve_thousand_archived_outputs_even_after_migration()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        for visual in [false, true] {
            let run = store.create_run(
                "Inspect",
                directory.path(),
                "custom",
                json!([]),
                json!({}),
                "",
            )?;
            store.state(&run.id, "running", json!({}))?;
            let operation =
                store.begin_operation(&run.id, "mcp.fixture.inspect", json!({}), true)?;
            crate::storage::claim_test_operation(&mut store, &operation)?;
            let result = if visual {
                json!({"content":[{"type":"image","mimeType":"image/png","data":encoded_png(b"scene")}]})
            } else {
                json!({"content":[{"type":"text","text":"plain output"}]})
            };
            crate::kernel::commit_result(&mut store, &operation, result, 1)?;
            let transaction = store.connection.unchecked_transaction()?;
            for _ in 0..12_000 {
                crate::storage::append_event(
                    &transaction,
                    &run.id,
                    "operation.output",
                    json!({"text":"output"}),
                )?;
            }
            transaction.commit()?;
            store.maintain_history(&run.id)?;
            store
                .connection
                .execute("DELETE FROM image_sources WHERE run_id=?1", [&run.id])?;
            store
                .connection
                .execute("DELETE FROM image_migrations WHERE run_id=?1", [&run.id])?;
            drop(store);
            store = Store::open(&root)?;
            assert_eq!(
                latest_successful_mcp_images(&mut store, &run.id)?.len(),
                usize::from(visual)
            );
            let original: String = store.connection.query_row(
                "SELECT artifact FROM event_archives WHERE run_id=?1 LIMIT 1",
                [&run.id],
                |row| row.get(0),
            )?;
            let corrupt_archive = store.put_artifact(b"not an event archive")?;
            store.connection.execute(
                "UPDATE event_archives SET artifact=?2 WHERE run_id=?1 AND artifact=?3",
                params![run.id, corrupt_archive, original],
            )?;
            assert!(
                store.events(&run.id).is_err(),
                "control: full replay reads the archive"
            );
            for _ in 0..10 {
                assert_eq!(
                    latest_successful_mcp_images(&mut store, &run.id)?.len(),
                    usize::from(visual)
                );
            }
            store.connection.execute(
                "UPDATE event_archives SET artifact=?2 WHERE run_id=?1 AND artifact=?3",
                params![run.id, original, corrupt_archive],
            )?;
        }
        Ok(())
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
        let images = latest_successful_mcp_images(&mut store, &run.id)?;
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
            latest_successful_mcp_images(&mut store, &run.id)?[0].artifact_hash,
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
            latest_successful_mcp_images(&mut store, &run.id)?[0].artifact_hash,
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
