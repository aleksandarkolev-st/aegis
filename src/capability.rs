use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::storage::Store;

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub id: String,
    pub version: u32,
    pub purpose: String,
    pub permission: String,
    pub side_effect: String,
    pub cost: u32,
    pub input_schema: Value,
}

pub fn registry() -> Vec<Manifest> {
    vec![
        Manifest {
            id: "workspace.search".into(),
            version: 1,
            purpose: "Find a literal substring in repository files. Set path to search one exact relative file, e.g. path=README.md and query=## for its headings.".into(),
            permission: "workspace.read".into(),
            side_effect: "none".into(),
            cost: 2,
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"query":{"type":"string"},"path":{"type":"string","minLength":1,"maxLength":128,"description":"Optional exact relative file; omit to search the workspace."}},"required":["query"]}),
        },
        Manifest {
            id: "workspace.read".into(),
            version: 1,
            purpose: "Read a UTF-8 workspace file. Complete files through 65536 Unicode characters enter context directly, ready to use without inspecting the artifact. Larger files remain artifact-backed; use read_batch for bounded ranges.".into(),
            permission: "workspace.read".into(),
            side_effect: "none".into(),
            cost: 1,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        },
        Manifest {
            id: "workspace.read_batch".into(),
            version: 1,
            purpose: "Read up to eight UTF-8 file ranges. Output automatically paginates at 65536 actual Unicode characters total; larger requested ranges are valid. Text enters context directly: use it without inspect_result. Continue only missing text using next_offset, not the previous offset. Distinct offsets of the same file are allowed.".into(),
            permission: "workspace.read".into(),
            side_effect: "none".into(),
            cost: 2,
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"files":{"type":"array","minItems":1,"maxItems":8,"uniqueItems":true,"items":{"type":"object","additionalProperties":false,"properties":{"path":{"type":"string","minLength":1,"maxLength":128},"offset":{"type":"integer","minimum":0,"maximum":2097152},"length":{"type":"integer","minimum":1,"maximum":65536,"description":"Requested Unicode characters; no aggregate-request arithmetic required. Output stops at 65536 actual characters total. next_offset is the first unread character; when a later file receives no text, resume at its unchanged offset."}},"required":["path","length"]}}},"required":["files"]}),
        },
        Manifest {
            id: "workspace.write".into(),
            version: 1,
            purpose: "Write exact UTF-8 content to a workspace file; return immediate read-back SHA256 and complete small text".into(),
            permission: "workspace.write".into(),
            side_effect: "workspace".into(),
            cost: 4,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        },
        Manifest {
            id: "workspace.patch".into(),
            version: 1,
            purpose: "Edit an existing UTF-8 file using small unique exact text replacements; preserve unrelated content".into(),
            permission: "workspace.write".into(),
            side_effect: "workspace".into(),
            cost: 3,
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"path":{"type":"string"},"expected_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"},"edits":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","additionalProperties":false,"properties":{"old":{"type":"string","minLength":1},"new":{"type":"string"}},"required":["old","new"]}}},"required":["path","edits"]}),
        },
        Manifest {
            id: "process.run".into(),
            version: 1,
            purpose: "Run a directly executed process with arguments in the workspace".into(),
            permission: "process.run".into(),
            side_effect: "unknown".into(),
            cost: 8,
            input_schema: json!({"type":"object","properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}},"required":["program","args"]}),
        },
        Manifest {
            id: "network.fetch".into(),
            version: 1,
            purpose: "Read a public HTTPS URL from an approved exact domain into an artifact"
                .into(),
            permission: "network.fetch".into(),
            side_effect: "none".into(),
            cost: 6,
            input_schema: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        },
    ]
}

pub fn all(store: &Store) -> Result<Vec<Manifest>> {
    let mut manifests = registry();
    for tool in store.mcp_tools()? {
        manifests.push(Manifest {
            id: format!("mcp.{}.{}", tool.server, tool.name),
            version: tool.version,
            purpose: tool.description,
            permission: format!("mcp:{}:{}", tool.server, tool.name),
            side_effect: "unknown".into(),
            cost: 8,
            input_schema: tool.input_schema,
        });
    }
    Ok(manifests)
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn resolve(
    store: &Store,
    query: &str,
    grants: &[String],
    limit: usize,
) -> Result<Vec<Manifest>> {
    let terms = words(query);
    let mut candidates: Vec<_> = all(store)?
        .into_iter()
        .filter(|manifest| grants.iter().any(|grant| grant == &manifest.permission))
        .map(|manifest| {
            let name = words(&manifest.id);
            let purpose = words(&manifest.purpose);
            let score: usize = terms
                .iter()
                .map(|term| {
                    if name.contains(term) {
                        3
                    } else if purpose.contains(term) {
                        1
                    } else {
                        0
                    }
                })
                .sum();
            (score, manifest)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    candidates.sort_by(|(score_a, manifest_a), (score_b, manifest_b)| {
        score_b
            .cmp(score_a)
            .then(manifest_a.cost.cmp(&manifest_b.cost))
            .then(manifest_a.id.cmp(&manifest_b.id))
    });
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|(_, manifest)| manifest)
        .collect())
}

pub fn permitted(store: &Store, id: &str, grants: &[String]) -> Result<Option<Manifest>> {
    Ok(all(store)?.into_iter().find(|manifest| {
        manifest.id == id && grants.iter().any(|grant| grant == &manifest.permission)
    }))
}

fn local_references_only(schema: &Value) -> bool {
    match schema {
        Value::Object(properties) => properties.iter().all(|(key, value)| {
            if key == "$ref" {
                value
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#'))
            } else {
                local_references_only(value)
            }
        }),
        Value::Array(items) => items.iter().all(local_references_only),
        _ => true,
    }
}

pub fn validate_arguments(manifest: &Manifest, arguments: &Value) -> Result<()> {
    if !local_references_only(&manifest.input_schema) {
        bail!("external JSON Schema references are not allowed");
    }
    let validator = jsonschema::validator_for(&manifest.input_schema)?;
    if let Err(error) = validator.validate(arguments) {
        bail!("invalid arguments for {}: {error}", manifest.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_before_ranking_overlapping_tools() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let results = resolve(
            &store,
            "workspace write file",
            &["workspace.read".into()],
            3,
        )
        .unwrap();
        assert!(
            !results
                .iter()
                .any(|manifest| manifest.id == "workspace.write")
        );
        assert!(
            permitted(&store, "workspace.write", &["workspace.read".into()])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve(&store, "read file", &["workspace.read".into()], 1).unwrap()[0].id,
            "workspace.read"
        );
    }

    #[test]
    fn validates_arguments_without_fetching_remote_schemas() {
        let read = registry()
            .into_iter()
            .find(|manifest| manifest.id == "workspace.read")
            .unwrap();
        assert!(validate_arguments(&read, &json!({"path":"Cargo.toml"})).is_ok());
        assert!(validate_arguments(&read, &json!({"path":42})).is_err());
        assert!(validate_arguments(&read, &json!({})).is_err());
        let mut remote = read;
        remote.input_schema = json!({"$ref":"https://example.com/schema.json"});
        assert!(validate_arguments(&remote, &json!({})).is_err());
    }
}
