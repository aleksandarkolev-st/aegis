//! Revalidate file receipts and scoped workspace fingerprints for fresh evidence.
use crate::{
    filesystem::FileScopes,
    storage::{Operation, Run, Store},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
    permissions: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn stamp(metadata: &fs::Metadata) -> Option<Stamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(Stamp {
        size: metadata.len(),
        modified: metadata.modified().ok()?,
        created: metadata.created().ok(),
        permissions: permission_bits(metadata),
        #[cfg(unix)]
        identity: (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ),
    })
}

#[derive(Default)]
pub(crate) struct Cache {
    files: HashMap<PathBuf, (Stamp, [u8; 32])>,
    bytes_hashed: u64,
    hits: u64,
    workspace: Option<String>,
    dirty: HashSet<PathBuf>,
}

impl Cache {
    fn load(&mut self, connection: &Connection, workspace: &str) -> Result<()> {
        if self.workspace.as_deref() == Some(workspace) {
            return Ok(());
        }
        self.files.clear();
        self.dirty.clear();
        let mut query=connection.prepare("SELECT path,stamp,digest FROM file_hash_cache WHERE workspace=?1 ORDER BY rowid DESC LIMIT 100000")?;
        let rows = query.query_map([workspace], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (path, metadata, digest) = row?;
            if let (Ok(stamp), Ok(bytes)) = (
                serde_json::from_str::<Stamp>(&metadata),
                hex::decode(&digest),
            ) {
                if let Ok(digest) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    self.files.insert(PathBuf::from(path), (stamp, digest));
                }
            }
        }
        self.workspace = Some(workspace.into());
        Ok(())
    }

    fn save(&mut self, connection: &Connection) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let Some(workspace) = self.workspace.as_deref() else {
            return Ok(());
        };
        let transaction = if connection.is_autocommit() {
            Some(connection.unchecked_transaction()?)
        } else {
            None
        };
        {
            let mut insert=connection.prepare("INSERT INTO file_hash_cache(workspace,path,stamp,digest) VALUES (?1,?2,?3,?4) ON CONFLICT(workspace,path) DO UPDATE SET stamp=excluded.stamp,digest=excluded.digest")?;
            for path in &self.dirty {
                if let Some((stamp, digest)) = self.files.get(path) {
                    insert.execute(params![
                        workspace,
                        path.to_string_lossy(),
                        serde_json::to_string(stamp)?,
                        hex::encode(digest)
                    ])?;
                }
            }
        }
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM file_hash_cache WHERE workspace=?1",
            [workspace],
            |row| row.get(0),
        )?;
        if count > MAX_WORKSPACE_FINGERPRINT_ENTRIES as i64 {
            connection.execute("DELETE FROM file_hash_cache WHERE workspace=?1 AND rowid NOT IN (SELECT rowid FROM file_hash_cache WHERE workspace=?1 ORDER BY rowid DESC LIMIT 100000)",[workspace])?;
        }
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        self.dirty.clear();
        Ok(())
    }
    fn get(&mut self, path: &Path, metadata: &fs::Metadata) -> Option<[u8; 32]> {
        let stamp = stamp(metadata)?;
        let (previous, digest) = self.files.get(path)?;
        if previous != &stamp {
            return None;
        }
        self.hits = self.hits.saturating_add(1);
        Some(*digest)
    }

    fn insert(&mut self, path: &Path, metadata: &fs::Metadata, digest: [u8; 32]) {
        if let Some(stamp) = stamp(metadata) {
            self.bytes_hashed = self.bytes_hashed.saturating_add(metadata.len());
            if self
                .files
                .get(path)
                .is_some_and(|(old, hash)| old == &stamp && hash == &digest)
            {
                return;
            }
            if self.files.len() >= MAX_WORKSPACE_FINGERPRINT_ENTRIES {
                self.files.clear();
            }
            self.files.insert(path.to_path_buf(), (stamp, digest));
            self.dirty.insert(path.to_path_buf());
        }
    }
}

const MAX_WORKSPACE_FINGERPRINT_ENTRIES: usize = 100_000;
const MAX_WORKSPACE_FINGERPRINT_BYTES: u64 = 512 * 1024 * 1024;
const WORKSPACE_FINGERPRINT_BUFFER_BYTES: usize = 64 * 1024;
pub(crate) const WORKSPACE_FINGERPRINT_SCOPE: &str = "Git-tracked and in-scope untracked files; ignored untracked files included except conventional generated-output directories; bounded scoped-tree fallback outside Git";

#[derive(Debug)]
struct FingerprintEntry {
    kind: u8,
    size: u64,
    permissions: u64,
    content_hash: Option<[u8; 32]>,
}

struct WorkspaceFingerprintBuilder<'a> {
    entries: BTreeMap<String, FingerprintEntry>,
    visited: HashSet<String>,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
    cache: Option<&'a mut Cache>,
    reuse_hashes: bool,
}

impl WorkspaceFingerprintBuilder<'_> {
    fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            visited: HashSet::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            cache: None,
            reuse_hashes: true,
        }
    }

    fn add_tree(&mut self, workspace: &Path, start: &Path) -> Result<()> {
        let mut pending = vec![start.to_path_buf()];
        while let Some(path) = pending.pop() {
            let relative = path.strip_prefix(workspace)?;
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.split('/').any(crate::filesystem::metadata_name) {
                continue;
            }
            let name = if relative.is_empty() {
                ".".to_owned()
            } else {
                crate::filesystem::path_name(&relative)?
            };
            if !self.visited.insert(name.clone()) {
                continue;
            }
            if self.visited.len() > self.max_entries {
                bail!(
                    "workspace fingerprint exceeds the {}-entry limit; narrow filesystem read scopes",
                    self.max_entries
                );
            }

            let metadata = fs::symlink_metadata(&path)?;
            if crate::filesystem::linked(&metadata) {
                bail!("workspace fingerprint cannot include links or junctions: {name}");
            }
            if metadata.is_dir() {
                self.entries.insert(
                    name,
                    FingerprintEntry {
                        kind: b'd',
                        size: 0,
                        permissions: permission_bits(&metadata),
                        content_hash: None,
                    },
                );
                let mut children = fs::read_dir(&path)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                children.sort();
                pending.extend(children.into_iter().rev());
                continue;
            }
            if !metadata.is_file() {
                bail!("workspace fingerprint found a non-regular file: {name}");
            }

            let mut file = File::open(&path)?;
            let before = file.metadata()?;
            if !before.is_file() {
                bail!("workspace file changed type while fingerprinting: {name}");
            }
            if self.bytes.saturating_add(before.len()) > self.max_bytes {
                bail!(
                    "workspace fingerprint exceeds the {}-byte limit; narrow filesystem read scopes",
                    self.max_bytes
                );
            }
            if let Some(digest) = self
                .cache
                .as_deref_mut()
                .filter(|_| self.reuse_hashes)
                .and_then(|cache| cache.get(&path, &before))
            {
                self.bytes += before.len();
                self.entries.insert(
                    name,
                    FingerprintEntry {
                        kind: b'f',
                        size: before.len(),
                        permissions: permission_bits(&before),
                        content_hash: Some(digest),
                    },
                );
                continue;
            }
            let mut content = Sha256::new();
            let mut buffer = [0; WORKSPACE_FINGERPRINT_BUFFER_BYTES];
            let mut size = 0_u64;
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                size = size
                    .checked_add(read as u64)
                    .context("workspace fingerprint byte count overflow")?;
                if self.bytes.saturating_add(size) > self.max_bytes {
                    bail!(
                        "workspace fingerprint exceeds the {}-byte limit; narrow filesystem read scopes",
                        self.max_bytes
                    );
                }
                content.update(&buffer[..read]);
            }
            let after = file.metadata()?;
            if size != before.len()
                || after.len() != before.len()
                || before.modified()? != after.modified()?
            {
                bail!("workspace file changed while fingerprinting: {name}");
            }
            self.bytes = self
                .bytes
                .checked_add(size)
                .context("workspace fingerprint byte count overflow")?;
            let digest: [u8; 32] = content.finalize().into();
            if let Some(cache) = self.cache.as_deref_mut() {
                cache.insert(&path, &after, digest);
            }
            self.entries.insert(
                name,
                FingerprintEntry {
                    kind: b'f',
                    size,
                    permissions: permission_bits(&after),
                    content_hash: Some(digest),
                },
            );
        }
        Ok(())
    }

    fn add_git_path(&mut self, workspace: &Path, relative: &str) -> Result<()> {
        let relative = crate::filesystem::path_name(relative)?;
        if self.visited.contains(&relative) {
            return Ok(());
        }
        let path = workspace.join(&relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => self.add_tree(workspace, &path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.add_missing(&relative)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn add_missing(&mut self, relative: &str) -> Result<()> {
        let relative = crate::filesystem::path_name(relative)?;
        if !self.visited.insert(relative.clone()) {
            return Ok(());
        }
        if self.visited.len() > self.max_entries {
            bail!(
                "workspace fingerprint exceeds the {}-entry limit; narrow filesystem read scopes",
                self.max_entries
            );
        }
        self.entries.insert(
            relative,
            FingerprintEntry {
                kind: b'm',
                size: 0,
                permissions: 0,
                content_hash: None,
            },
        );
        Ok(())
    }

    fn finish(self) -> String {
        let mut fingerprint = Sha256::new();
        fingerprint.update(b"aegis-workspace-fingerprint-v1\0");
        for (path, entry) in self.entries {
            fingerprint.update((path.len() as u64).to_le_bytes());
            fingerprint.update(path.as_bytes());
            fingerprint.update([entry.kind]);
            fingerprint.update(entry.size.to_le_bytes());
            fingerprint.update(entry.permissions.to_le_bytes());
            if let Some(content_hash) = entry.content_hash {
                fingerprint.update(content_hash);
            }
        }
        hex::encode(fingerprint.finalize())
    }
}

#[cfg(unix)]
fn permission_bits(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() as u64
}

#[cfg(windows)]
fn permission_bits(metadata: &fs::Metadata) -> u64 {
    u64::from(metadata.permissions().readonly())
}

#[cfg(not(any(unix, windows)))]
fn permission_bits(_: &fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
pub(crate) fn workspace_fingerprint(run: &Run) -> Result<String> {
    workspace_fingerprint_with_limits(
        run,
        MAX_WORKSPACE_FINGERPRINT_ENTRIES,
        MAX_WORKSPACE_FINGERPRINT_BYTES,
    )
}

#[cfg(test)]
fn workspace_fingerprint_with_limits(
    run: &Run,
    max_entries: usize,
    max_bytes: u64,
) -> Result<String> {
    fingerprint_with_builder(
        run,
        WorkspaceFingerprintBuilder::new(max_entries, max_bytes),
        max_entries,
    )
}

#[cfg(test)]
fn workspace_fingerprint_cached(run: &Run, cache: &mut Cache) -> Result<String> {
    workspace_fingerprint_scanned(run, cache, true)
}

fn workspace_fingerprint_scanned(
    run: &Run,
    cache: &mut Cache,
    reuse_hashes: bool,
) -> Result<String> {
    let mut builder = WorkspaceFingerprintBuilder::new(
        MAX_WORKSPACE_FINGERPRINT_ENTRIES,
        MAX_WORKSPACE_FINGERPRINT_BYTES,
    );
    builder.cache = Some(cache);
    builder.reuse_hashes = reuse_hashes;
    fingerprint_with_builder(run, builder, MAX_WORKSPACE_FINGERPRINT_ENTRIES)
}

fn fingerprint_with_builder(
    run: &Run,
    mut builder: WorkspaceFingerprintBuilder<'_>,
    max_entries: usize,
) -> Result<String> {
    let workspace = Path::new(&run.workspace);
    let scopes = crate::filesystem::FileScopes::from_configuration(&run.budgets)?;
    let mut read = scopes
        .as_ref()
        .map(|scopes| scopes.read.clone())
        .unwrap_or_else(|| vec!["**".to_owned()]);
    read.sort();
    read.dedup();

    if let Some(files) = git_worktree_files(workspace, &read, max_entries)? {
        for relative in files {
            if relative.split('/').any(crate::filesystem::metadata_name) {
                continue;
            }
            let relative = crate::filesystem::path_name(&relative)?;
            if scopes
                .as_ref()
                .is_some_and(|scopes| !scopes.permits(&relative, false))
            {
                continue;
            }
            builder.add_git_path(workspace, &relative)?;
        }
    } else if let Some(scopes) = scopes {
        for pattern in read {
            let relative = if pattern == "**" {
                ""
            } else {
                pattern.strip_suffix("/**").unwrap_or(&pattern)
            };
            let target = if relative.is_empty() {
                workspace.to_path_buf()
            } else {
                scopes.checked_path(workspace, relative, false)?
            };
            if (pattern == "**" || pattern.ends_with("/**"))
                && !fs::symlink_metadata(&target)?.is_dir()
            {
                bail!("directory filesystem scopes must name existing directories");
            }
            if pattern != "**"
                && !pattern.ends_with("/**")
                && !fs::symlink_metadata(&target)?.is_file()
            {
                bail!("exact filesystem scopes must name existing files");
            }
            builder.add_tree(workspace, &target)?;
        }
    } else {
        builder.add_tree(workspace, workspace)?;
    }
    Ok(builder.finish())
}

fn git_worktree_files(
    workspace: &Path,
    read_scopes: &[String],
    max_entries: usize,
) -> Result<Option<Vec<String>>> {
    let root = match Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return Ok(None),
    };
    let root = std::str::from_utf8(&root.stdout)?.trim();
    let root = dunce::canonicalize(root)?;
    let workspace = dunce::canonicalize(workspace)?;
    let prefix = workspace
        .strip_prefix(&root)
        .context("Git workspace is not inside its repository root")?
        .to_string_lossy()
        .replace('\\', "/");
    let prefix = prefix.trim_matches('/');
    let prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };

    if read_scopes.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let regular = git_list_files(
        &workspace,
        &prefix,
        read_scopes,
        &["--cached", "--others", "--exclude-standard"],
        max_entries,
        false,
    )?;
    let ignored = git_list_files(
        &workspace,
        &prefix,
        read_scopes,
        &["--others", "--ignored", "--exclude-standard"],
        max_entries,
        true,
    )?;
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for path in regular.into_iter().chain(ignored) {
        if seen.insert(path.clone()) {
            if files.len() >= max_entries {
                bail!(
                    "workspace fingerprint exceeds the {}-entry limit; narrow filesystem read scopes",
                    max_entries
                );
            }
            files.push(path);
        }
    }
    files.sort();
    Ok(Some(files))
}

fn git_list_files(
    workspace: &Path,
    prefix: &str,
    read_scopes: &[String],
    modes: &[&str],
    max_entries: usize,
    skip_generated_ignored: bool,
) -> Result<Vec<String>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(["-c", "core.fsmonitor=false", "ls-files", "--full-name"])
        .args(modes)
        .arg("-z")
        .arg("--");
    if read_scopes.iter().any(|scope| scope == "**") {
        if !prefix.is_empty() {
            command.arg(format!(":(top,glob){prefix}**"));
        }
    } else {
        for scope in read_scopes {
            if scope.ends_with("/**") {
                command.arg(format!(":(top,glob){prefix}{scope}"));
            } else {
                command.arg(format!(":(top,literal){prefix}{scope}"));
            }
        }
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("enumerating scoped Git workspace files")?;
    let stdout = child
        .stdout
        .take()
        .context("Git workspace enumeration has no output stream")?;
    let mut reader = BufReader::new(stdout);
    let mut entry = Vec::new();
    let mut files = Vec::new();
    loop {
        entry.clear();
        if reader.read_until(0, &mut entry)? == 0 {
            break;
        }
        if entry.last() == Some(&0) {
            entry.pop();
        }
        if entry.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(&entry)?;
        let Some(path) = path.strip_prefix(prefix) else {
            continue;
        };
        if path.is_empty()
            || path.split('/').any(crate::filesystem::metadata_name)
            || (skip_generated_ignored && generated_output_path(path))
        {
            continue;
        }
        if files.len() >= max_entries {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "workspace fingerprint exceeds the {}-entry limit; narrow filesystem read scopes",
                max_entries
            );
        }
        files.push(path.to_owned());
    }
    if !child.wait()?.success() {
        bail!("Git could not enumerate workspace files; evidence cannot be verified");
    }
    Ok(files)
}

fn generated_output_path(path: &str) -> bool {
    path.split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".arun"
                | ".aegis"
                | "target"
                | "node_modules"
                | ".next"
                | ".nuxt"
                | ".turbo"
                | "dist"
                | "build"
                | "out"
                | "coverage"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".venv"
                | "venv"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(workspace: &Path, read: &[&str]) -> Run {
        Run {
            id: "fingerprint-test".into(),
            task: "test scoped workspace fingerprints".into(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: "test".into(),
            grants: json!(["process.run"]),
            budgets: json!({
                "filesystem_scopes": {
                    "read": read,
                    "write": []
                }
            }),
            acceptance: String::new(),
            state: "running".into(),
            created_at: 0,
        }
    }

    #[test]
    fn cached_workspace_scans_hash_only_changed_content_and_keep_strict_fingerprints() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        for index in 0..64 {
            fs::write(
                directory.path().join(format!("source-{index}.txt")),
                vec![index as u8; 32 * 1024],
            )?;
        }
        let run = run(directory.path(), &["**"]);
        let mut cache = Cache::default();
        let first = workspace_fingerprint_cached(&run, &mut cache)?;
        assert_eq!(cache.bytes_hashed, 64 * 32 * 1024);
        let bytes = cache.bytes_hashed;
        assert_eq!(workspace_fingerprint_cached(&run, &mut cache)?, first);
        assert_eq!(cache.bytes_hashed, bytes);
        assert_eq!(cache.hits, 64);
        let changed = directory.path().join("source-0.txt");
        let modified = fs::metadata(&changed)?.modified()?;
        fs::write(&changed, vec![255; 32 * 1024])?;
        fs::OpenOptions::new()
            .write(true)
            .open(&changed)?
            .set_times(
                fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(1)),
            )?;
        let next = workspace_fingerprint_cached(&run, &mut cache)?;
        assert_ne!(next, first);
        assert_eq!(cache.bytes_hashed - bytes, 32 * 1024);
        assert_eq!(next, workspace_fingerprint(&run)?);
        fs::write(directory.path().join("new.txt"), "new source")?;
        let added = workspace_fingerprint_cached(&run, &mut cache)?;
        assert_ne!(added, next);
        fs::remove_file(directory.path().join("new.txt"))?;
        assert_eq!(workspace_fingerprint_cached(&run, &mut cache)?, next);
        println!(
            "background fingerprint fixture: initial bytes hashed={bytes}; unchanged scan=0; one changed file={}",
            32 * 1024
        );
        Ok(())
    }

    #[test]
    fn strict_proof_validation_rejects_same_size_edits_with_restored_timestamps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let source = directory.path().join("source.txt");
        fs::write(&source, "old bytes")?;
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Read source",
            directory.path(),
            "custom",
            json!(["workspace.read"]),
            json!({"obligations":["Read current source"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"source.txt"}),
            true,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.claim_operation(&operation)?;
        let artifact = store.put_artifact(&serde_json::to_vec(
            &json!({"content":"old bytes","sha256":hex::encode(Sha256::digest(b"old bytes"))}),
        )?)?;
        store.operation_state(&operation, "succeeded", Some(&artifact), json!({}))?;
        store.verify_obligation(&run.id, 1, &[artifact.clone()])?;
        assert!(!store.refresh_observed_files_cached(&run.id)?);
        let modified = fs::metadata(&source)?.modified()?;
        fs::write(&source, "new bytes")?;
        fs::OpenOptions::new()
            .write(true)
            .open(&source)?
            .set_times(fs::FileTimes::new().set_modified(modified))?;
        #[cfg(windows)]
        assert!(
            !store.refresh_observed_files_cached(&run.id)?,
            "This fixture must exercise a provisional metadata cache hit"
        );
        assert!(
            store
                .verify_obligation(&run.id, 1, &[artifact.clone()])
                .is_err()
        );
        assert!(store.complete_run(&run.id, "Done", &[artifact]).is_err());
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        Ok(())
    }

    #[test]
    fn cold_worker_claims_reuse_persisted_hashes_and_exploration_exceeds_1024_paths() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Explore many sources",
            directory.path(),
            "custom",
            json!(["workspace.read"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let mut selected = Vec::new();
        let content = "source bytes";
        let digest = hex::encode(Sha256::digest(content.as_bytes()));
        for index in 0..1056 {
            let path = format!("source-{index}.txt");
            fs::write(directory.path().join(&path), content)?;
            selected.push(json!({"path":path,"sha256":digest,"content":content,"offset":0,"next_offset":content.len(),"total_characters":content.len()}));
            if selected.len() == 32 {
                let operation=store.begin_operation(&run.id,"workspace.read_batch",json!({"selections":selected.iter().map(|entry|json!({"path":entry["path"],"offset":0,"length":content.len()})).collect::<Vec<_>>()}),true)?;
                store.operation_state(&operation, "dispatched", None, json!({}))?;
                store.claim_operation(&operation)?;
                let artifact =
                    store.put_artifact(&serde_json::to_vec(&json!({"selected":selected}))?)?;
                store.operation_state(&operation, "succeeded", Some(&artifact), json!({}))?;
                selected.clear();
            }
        }
        assert!(!store.refresh_observed_files_cached(&run.id)?);
        let count: i64 = store.connection.query_row(
            "SELECT COUNT(*) FROM observed_files WHERE run_id=?1",
            [&run.id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1056);
        drop(store);
        let mut store = Store::open(&root)?;
        let next = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"source-0.txt"}),
            true,
        )?;
        store.operation_state(&next, "dispatched", None, json!({}))?;
        store.claim_operation(&next)?;
        assert_eq!(
            store.freshness_cache.borrow().bytes_hashed,
            0,
            "A cold worker must not rehash the accumulated read ledger"
        );
        assert_eq!(store.freshness_cache.borrow().hits, 1056);
        assert!(store.changed_observed_files(&run.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn scoped_workspace_fingerprint_detects_changes_additions_and_deletions() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path();
        fs::create_dir_all(workspace.join("src"))?;
        fs::create_dir_all(workspace.join("docs"))?;
        fs::write(workspace.join("src/lib.rs"), b"initial source")?;
        fs::write(workspace.join("docs/outside.bin"), vec![9; 16 * 1024])?;
        let run = run(workspace, &["src/**"]);

        let initial = workspace_fingerprint(&run)?;
        assert_eq!(workspace_fingerprint(&run)?, initial);

        fs::write(workspace.join("src/lib.rs"), b"edited source")?;
        let edited = workspace_fingerprint(&run)?;
        assert_ne!(edited, initial);

        fs::write(workspace.join("src/new.rs"), b"new source")?;
        let added = workspace_fingerprint(&run)?;
        assert_ne!(added, edited);

        fs::remove_file(workspace.join("src/new.rs"))?;
        assert_eq!(workspace_fingerprint(&run)?, edited);

        // A large out-of-scope file is neither read nor charged against the cap.
        assert_eq!(workspace_fingerprint_with_limits(&run, 10, 64)?, edited);
        Ok(())
    }

    #[test]
    fn workspace_fingerprint_fails_closed_on_entry_and_byte_caps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path();
        fs::create_dir_all(workspace.join("src"))?;
        fs::write(workspace.join("src/lib.rs"), b"some source bytes")?;
        let run = run(workspace, &["src/**"]);

        assert!(workspace_fingerprint_with_limits(&run, 1, u64::MAX).is_err());
        assert!(workspace_fingerprint_with_limits(&run, 10, 4).is_err());
        Ok(())
    }

    #[test]
    fn exact_file_fingerprint_excludes_siblings_and_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path();
        fs::create_dir_all(workspace.join("src"))?;
        fs::create_dir_all(workspace.join(".git"))?;
        fs::write(workspace.join("src/lib.rs"), b"selected")?;
        fs::write(workspace.join("src/other.rs"), b"sibling")?;
        fs::write(workspace.join(".git/index"), b"metadata")?;
        let run = run(workspace, &["src/lib.rs"]);

        let initial = workspace_fingerprint(&run)?;
        fs::write(workspace.join("src/other.rs"), b"changed sibling")?;
        fs::write(workspace.join(".git/index"), b"changed metadata")?;
        assert_eq!(workspace_fingerprint(&run)?, initial);
        Ok(())
    }

    #[test]
    fn git_fingerprint_includes_tracked_and_unignored_scoped_files_but_skips_ignored_outputs()
    -> Result<()> {
        if Command::new("git").arg("--version").output().is_err() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let workspace = directory.path();
        fs::create_dir_all(workspace.join("src/target"))?;
        fs::create_dir_all(workspace.join("docs"))?;
        fs::write(
            workspace.join(".gitignore"),
            "src/target/\nsrc/secret.fixture\n",
        )?;
        fs::write(workspace.join("src/tracked.rs"), b"source")?;
        fs::write(workspace.join("src/target/cache.bin"), vec![1; 16 * 1024])?;
        fs::write(workspace.join("src/secret.fixture"), b"private input")?;
        fs::write(workspace.join("docs/outside.bin"), vec![2; 16 * 1024])?;
        let init = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["init", "--quiet"])
            .status()?;
        assert!(init.success());
        let add = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["add", "--", ".gitignore", "src/tracked.rs"])
            .status()?;
        assert!(add.success());

        let run = run(workspace, &["src/**"]);
        let initial = workspace_fingerprint_with_limits(&run, 20, 64)?;
        assert_eq!(workspace_fingerprint_with_limits(&run, 20, 64)?, initial);

        fs::write(workspace.join("src/tracked.rs"), b"edited")?;
        let edited = workspace_fingerprint_with_limits(&run, 20, 64)?;
        assert_ne!(edited, initial);

        fs::write(workspace.join("src/new.rs"), b"untracked source")?;
        let added = workspace_fingerprint_with_limits(&run, 20, 64)?;
        assert_ne!(added, edited);

        fs::remove_file(workspace.join("src/new.rs"))?;
        assert_eq!(workspace_fingerprint_with_limits(&run, 20, 64)?, edited);

        fs::write(
            workspace.join("src/secret.fixture"),
            b"changed private input",
        )?;
        assert_ne!(workspace_fingerprint_with_limits(&run, 20, 64)?, edited);
        Ok(())
    }

    #[test]
    fn nested_git_workspace_uses_scoped_tracked_files_and_ignores_build_output() -> Result<()> {
        if Command::new("git").arg("--version").output().is_err() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let repository = directory.path();
        let workspace = repository.join("project");
        fs::create_dir_all(workspace.join("src/target"))?;
        fs::create_dir_all(repository.join("outside"))?;
        fs::write(repository.join(".gitignore"), "project/src/target/\n")?;
        fs::write(workspace.join("src/lib.rs"), b"nested source")?;
        fs::write(workspace.join("src/target/cache.bin"), vec![3; 16 * 1024])?;
        fs::write(repository.join("outside/huge.bin"), vec![4; 16 * 1024])?;
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["init", "--quiet"])
                .status()?
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["add", "--", ".gitignore", "project/src/lib.rs"])
                .status()?
                .success()
        );

        let run = run(&workspace, &["src/**"]);
        let initial = workspace_fingerprint_with_limits(&run, 20, 64)?;
        assert_eq!(workspace_fingerprint_with_limits(&run, 20, 64)?, initial);
        fs::write(workspace.join("src/lib.rs"), b"edited nested source")?;
        assert_ne!(workspace_fingerprint_with_limits(&run, 20, 64)?, initial);
        Ok(())
    }
}

pub(crate) fn record(
    transaction: &Transaction<'_>,
    operation: &Operation,
    result: Option<&Value>,
) -> Result<()> {
    if !matches!(
        operation.capability.as_str(),
        "workspace.read" | "workspace.read_batch" | "workspace.write" | "workspace.patch"
    ) {
        return Ok(());
    }
    let Some(result) = result else {
        return Ok(());
    };
    let entries: Vec<&Value> = if operation.capability == "workspace.read_batch" {
        result["selected"]
            .as_array()
            .into_iter()
            .flatten()
            .collect()
    } else {
        vec![result]
    };
    for entry in entries {
        let path = entry["path"].as_str().or_else(|| {
            (operation.capability != "workspace.read_batch")
                .then(|| operation.arguments["path"].as_str())
                .flatten()
        });
        let (Some(path), Some(hash)) = (path, entry["sha256"].as_str()) else {
            continue;
        };
        let path = crate::filesystem::path_name(path)?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("File receipt has an invalid content hash");
        }
        let previous: Option<String> = transaction
            .query_row(
                "SELECT hash FROM observed_files WHERE run_id=?1 AND path=?2",
                params![operation.run_id, path],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref().is_some_and(|old| old != hash)
            && matches!(
                operation.capability.as_str(),
                "workspace.read" | "workspace.read_batch"
            )
        {
            crate::obligations::invalidate_workspace(
                transaction,
                &operation.run_id,
                json!({"reason":"observed file changed since its prior receipt","path":path}),
            )?;
        }
        transaction.execute("INSERT INTO observed_files(run_id,path,hash,changed_seq) VALUES (?1,?2,?3,(SELECT last_seq FROM run_projection WHERE run_id=?1)) ON CONFLICT(run_id,path) DO UPDATE SET changed_seq=CASE WHEN hash!=excluded.hash THEN excluded.changed_seq ELSE changed_seq END,hash=excluded.hash",params![operation.run_id,path,hash])?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn changed_observed_files(&self, id: &str) -> Result<Vec<(String, String, String)>> {
        self.changed_observed_files_mode(id, false)
    }

    fn changed_observed_files_mode(
        &self,
        id: &str,
        cached: bool,
    ) -> Result<Vec<(String, String, String)>> {
        let run = self.run(id)?;
        if run.is_terminal() {
            return Ok(Vec::new());
        }
        self.freshness_cache
            .borrow_mut()
            .load(&self.connection, &run.workspace)?;
        let sources = {
            let mut statement = self
                .connection
                .prepare("SELECT path,hash FROM observed_files WHERE run_id=?1 ORDER BY path")?;
            statement
                .query_map([id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let scopes = FileScopes::from_configuration(&run.budgets)?.unwrap_or(FileScopes {
            read: vec!["**".into()],
            write: vec![],
        });
        let mut changed = Vec::new();
        for (path, expected) in sources {
            let digest = (|| -> Result<String> {
                let target = scopes.checked_path(Path::new(&run.workspace), &path, false)?;
                let file = File::open(&target)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
                    bail!("Observed source is no longer a bounded regular file");
                }
                if cached {
                    if let Some(digest) = self.freshness_cache.borrow_mut().get(&target, &metadata)
                    {
                        return Ok(hex::encode(digest));
                    }
                }
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 2 * 1024 * 1024 {
                    bail!("Observed source grew beyond its bound");
                }
                let after = fs::metadata(&target)?;
                if after.len() != metadata.len() || after.modified()? != metadata.modified()? {
                    bail!("Observed source changed while checking freshness")
                }
                let digest: [u8; 32] = Sha256::digest(&bytes).into();
                self.freshness_cache
                    .borrow_mut()
                    .insert(&target, &after, digest);
                Ok(hex::encode(digest))
            })();
            let actual = digest.unwrap_or_else(|_| "unavailable".into());
            if actual != expected {
                changed.push((path, expected, actual));
            }
        }
        self.freshness_cache.borrow_mut().save(&self.connection)?;
        Ok(changed)
    }

    pub fn refresh_observed_files(&mut self, id: &str) -> Result<bool> {
        self.refresh_observed_files_mode(id, false)
    }

    // Cached scans are provisional. Proof verification and completion use the
    // strict entry point and never trust unchanged timestamps as evidence.
    pub(crate) fn refresh_observed_files_cached(&mut self, id: &str) -> Result<bool> {
        self.refresh_observed_files_mode(id, true)
    }

    fn refresh_observed_files_mode(&mut self, id: &str, cached: bool) -> Result<bool> {
        let workspace_changed = self.refresh_workspace_fingerprint_mode(id, cached)?;
        let changed = self.changed_observed_files_mode(id, cached)?;
        if changed.is_empty() {
            return Ok(workspace_changed);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut paths = Vec::new();
        for (path, expected, actual) in changed {
            let count = transaction.execute(
                "UPDATE observed_files SET hash=?4,changed_seq=(SELECT last_seq FROM run_projection WHERE run_id=?1) WHERE run_id=?1 AND path=?2 AND hash=?3",
                params![id, path, expected, actual],
            )?;
            if count > 0 {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            return Ok(false);
        }
        if !workspace_changed {
            crate::obligations::invalidate_workspace(
                &transaction,
                id,
                json!({"reason":"observed files changed outside their recorded file-tool receipts","paths":paths}),
            )?;
        }
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn check_observed_files(&self, id: &str) -> Result<()> {
        if !self.changed_observed_files(id)?.is_empty() {
            bail!("Observed workspace files changed; gather fresh evidence before completion");
        }
        self.check_workspace_fingerprint(id)
    }

    pub(crate) fn capture_workspace_fingerprint(&self, id: &str) -> Result<String> {
        self.scan_workspace_fingerprint(id, false)
    }

    fn scan_workspace_fingerprint(&self, id: &str, cached: bool) -> Result<String> {
        let run = self.run(id)?;
        let mut cache = self.freshness_cache.borrow_mut();
        cache.load(&self.connection, &run.workspace)?;
        let fingerprint = workspace_fingerprint_scanned(&run, &mut cache, cached)?;
        cache.save(&self.connection)?;
        Ok(fingerprint)
    }

    pub(crate) fn workspace_fingerprint_baseline(&self, id: &str) -> Result<Option<(String, i64)>> {
        workspace_fingerprint_baseline(&self.connection, id)
    }

    fn refresh_workspace_fingerprint_mode(&mut self, id: &str, cached: bool) -> Result<bool> {
        let baseline = self.workspace_fingerprint_baseline(id)?;
        if baseline.is_none() && !workspace_has_process_evidence(&self.connection, id)? {
            return Ok(false);
        }
        let fingerprint = self.scan_workspace_fingerprint(id, cached)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = workspace_fingerprint_baseline(&transaction, id)?;
        let has_process_evidence = workspace_has_process_evidence(&transaction, id)?;
        if current.is_none() && !has_process_evidence {
            transaction.commit()?;
            return Ok(false);
        }
        let changed = current
            .as_ref()
            .is_none_or(|(current_expected, _)| fingerprint != *current_expected);
        if changed {
            crate::obligations::invalidate_workspace(
                &transaction,
                id,
                json!({"reason":if current.is_none(){"pre-upgrade process evidence had no workspace fingerprint"}else{"scoped workspace content changed outside a verified Aegis write"},"fingerprint_scope":WORKSPACE_FINGERPRINT_SCOPE}),
            )?;
        }
        let revision = workspace_revision(&transaction, id)?;
        save_workspace_fingerprint(&transaction, id, &fingerprint, revision)?;
        transaction.commit()?;
        Ok(changed)
    }

    pub(crate) fn check_workspace_fingerprint(&self, id: &str) -> Result<()> {
        let Some((expected, baseline_revision)) = self.workspace_fingerprint_baseline(id)? else {
            if workspace_has_process_evidence(&self.connection, id)? {
                bail!(
                    "Existing process evidence predates scoped workspace fingerprinting; refresh the workspace and gather fresh evidence before verifying or completing"
                );
            }
            return Ok(());
        };
        let actual = self.capture_workspace_fingerprint(id)?;
        let revision = workspace_revision(&self.connection, id)?;
        if actual != expected {
            bail!(
                "Scoped workspace content changed since process evidence was captured; run process.run again before verifying or completing"
            );
        }
        if revision != baseline_revision {
            bail!(
                "Scoped workspace fingerprint is not synchronized with the current workspace revision; refresh evidence before verifying or completing"
            );
        }
        Ok(())
    }
}

pub(crate) fn workspace_fingerprint_baseline(
    connection: &Connection,
    id: &str,
) -> Result<Option<(String, i64)>> {
    connection
        .query_row(
            "SELECT fingerprint, revision FROM workspace_fingerprints WHERE run_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn workspace_has_process_evidence(connection: &Connection, id: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE run_id=?1 AND state='succeeded' AND (capability='process.run' OR (capability LIKE 'mcp.%' AND EXISTS(SELECT 1 FROM runs,json_each(runs.grants) AS grant_item WHERE runs.id=?1 AND grant_item.value='workspace.write'))))",
            [id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

pub(crate) fn workspace_revision(connection: &Connection, id: &str) -> Result<i64> {
    connection
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id=?1",
            [id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

pub(crate) fn save_workspace_fingerprint(
    transaction: &Transaction<'_>,
    id: &str,
    fingerprint: &str,
    revision: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO workspace_fingerprints(run_id,fingerprint,revision) VALUES (?1,?2,?3)
         ON CONFLICT(run_id) DO UPDATE SET fingerprint=excluded.fingerprint,revision=excluded.revision",
        params![id, fingerprint, revision],
    )?;
    Ok(())
}
