use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::filesystem::FileScopes;
use crate::storage::Run;

pub(crate) const TEXT_BUDGET: usize = 65_536;
const MAX_REQUEST: usize = 65_536;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    files: Vec<Slice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slice {
    path: String,
    #[serde(default)]
    offset: usize,
    length: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub path: String,
    pub sha256: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total_characters: usize,
    pub text: String,
}

fn request(arguments: &Value) -> Result<Request> {
    let request: Request = serde_json::from_value(arguments.clone())?;
    if request.files.is_empty() || request.files.len() > 8 {
        bail!("read batch needs one to eight requested files");
    }
    let mut paths = std::collections::HashSet::new();
    for slice in &request.files {
        if slice.path.len() > 128 || crate::filesystem::path_name(&slice.path)? != slice.path {
            bail!("batch paths need portable forward-slash names of at most 128 UTF-8 bytes");
        }
        if !paths.insert((&slice.path, slice.offset)) {
            bail!(
                "duplicate read range for {} at offset {}; request each starting range once",
                slice.path,
                slice.offset
            );
        }
        if slice.length == 0 || slice.length > MAX_REQUEST {
            bail!(
                "requested length for {} must be 1..65536 Unicode characters; output is paginated automatically",
                slice.path
            );
        }
        if slice.offset > 2 * 1024 * 1024 {
            bail!(
                "offset for {} exceeds 2097152 Unicode characters",
                slice.path
            );
        }
    }
    Ok(request)
}

pub(crate) fn authorize(run: &Run, arguments: &Value) -> Result<()> {
    let request = request(arguments)?;
    let scopes = FileScopes::from_configuration(&run.budgets)?.unwrap_or(FileScopes {
        read: vec!["**".into()],
        write: Vec::new(),
    });
    for slice in &request.files {
        let path = scopes.checked_path(Path::new(&run.workspace), &slice.path, false)?;
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
            bail!("batch sources must be regular UTF-8 files of at most 2 MiB");
        }
    }
    Ok(())
}

pub(crate) fn read(run: &Run, arguments: &Value) -> Result<Value> {
    authorize(run, arguments)?;
    let request = request(arguments)?;
    let scopes = FileScopes::from_configuration(&run.budgets)?.unwrap_or(FileScopes {
        read: vec!["**".into()],
        write: Vec::new(),
    });
    let mut selected = Vec::new();
    let mut remaining = TEXT_BUDGET;
    for slice in request.files {
        let path = scopes.checked_path(Path::new(&run.workspace), &slice.path, false)?;
        let mut content = String::new();
        File::open(path)?
            .take(2 * 1024 * 1024 + 1)
            .read_to_string(&mut content)?;
        if content.len() > 2 * 1024 * 1024 {
            bail!("batch source grew beyond 2 MiB during the read");
        }
        let total_characters = content.chars().count();
        if slice.offset > total_characters {
            bail!("requested offset is past the end of {}", slice.path);
        }
        let text: String = content
            .chars()
            .skip(slice.offset)
            .take(slice.length.min(remaining))
            .collect();
        remaining -= text.chars().count();
        selected.push(Selection {
            path: slice.path,
            sha256: hex::encode(Sha256::digest(content.as_bytes())),
            offset: slice.offset,
            next_offset: slice.offset + text.chars().count(),
            total_characters,
            text,
        });
    }
    let result = serde_json::json!({"selected":selected});
    mapped(&result)?;
    Ok(result)
}

pub(crate) fn mapped(result: &Value) -> Result<Value> {
    if serde_json::to_vec(result)?.len() > 524288 {
        bail!("selected read result exceeds 524288 serialized UTF-8 bytes");
    }
    let selections: Vec<Selection> = serde_json::from_value(result["selected"].clone())
        .context("invalid selected read result")?;
    if selections.is_empty() || selections.len() > 8 {
        bail!("invalid selected read count");
    }
    let mut characters = 0;
    let mut paths = std::collections::HashSet::new();
    for selection in &selections {
        if selection.path.len() > 128
            || crate::filesystem::path_name(&selection.path)? != selection.path
            || !paths.insert((&selection.path, selection.offset))
        {
            bail!("invalid selected read path");
        }
        if selection.sha256.len() != 64
            || !selection
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("invalid selected file digest");
        }
        let length = selection.text.chars().count();
        if selection.offset.checked_add(length) != Some(selection.next_offset)
            || selection.next_offset > selection.total_characters
            || selection.total_characters > 2 * 1024 * 1024
        {
            bail!("invalid selected read offsets");
        }
        characters += length;
    }
    if characters > TEXT_BUDGET {
        bail!("selected read text exceeds 65536 Unicode characters");
    }
    Ok(serde_json::to_value(selections)?)
}
