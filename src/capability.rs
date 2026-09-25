use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub id: &'static str,
    pub version: u32,
    pub purpose: &'static str,
    pub permission: &'static str,
    pub side_effect: &'static str,
    pub cost: u32,
    pub input_schema: Value,
}

pub fn registry() -> Vec<Manifest> {
    vec![
        Manifest {
            id: "workspace.search",
            version: 1,
            purpose: "Find text in repository files by literal substring",
            permission: "workspace.read",
            side_effect: "none",
            cost: 2,
            input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        },
        Manifest {
            id: "workspace.read",
            version: 1,
            purpose: "Read a UTF-8 workspace file by relative path",
            permission: "workspace.read",
            side_effect: "none",
            cost: 1,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        },
        Manifest {
            id: "workspace.write",
            version: 1,
            purpose: "Write exact UTF-8 content to a workspace file",
            permission: "workspace.write",
            side_effect: "workspace",
            cost: 4,
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        },
        Manifest {
            id: "process.run",
            version: 1,
            purpose: "Run a directly executed process with arguments in the workspace",
            permission: "process.run",
            side_effect: "unknown",
            cost: 8,
            input_schema: json!({"type":"object","properties":{"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}}},"required":["program","args"]}),
        },
    ]
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn resolve(query: &str, grants: &[String], limit: usize) -> Vec<Manifest> {
    let terms = words(query);
    let mut candidates: Vec<_> = registry()
        .into_iter()
        .filter(|manifest| grants.iter().any(|grant| grant == manifest.permission))
        .map(|manifest| {
            let name = words(manifest.id);
            let purpose = words(manifest.purpose);
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
            .then(manifest_a.id.cmp(manifest_b.id))
    });
    candidates
        .into_iter()
        .take(limit)
        .map(|(_, manifest)| manifest)
        .collect()
}

pub fn permitted(id: &str, grants: &[String]) -> Option<Manifest> {
    registry().into_iter().find(|manifest| {
        manifest.id == id && grants.iter().any(|grant| grant == manifest.permission)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_before_ranking_overlapping_tools() {
        let results = resolve("workspace write file", &["workspace.read".into()], 3);
        assert!(
            !results
                .iter()
                .any(|manifest| manifest.id == "workspace.write")
        );
        assert!(permitted("workspace.write", &["workspace.read".into()]).is_none());
        assert_eq!(
            resolve("read file", &["workspace.read".into()], 1)[0].id,
            "workspace.read"
        );
    }
}
