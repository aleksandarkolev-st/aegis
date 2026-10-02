//! Revalidate file receipts and scoped workspace fingerprints for fresh evidence.
use crate::{
    filesystem::FileScopes,
    storage::{Operation, Run, Store},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Command, Stdio},
};

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

struct WorkspaceFingerprintBuilder {
    entries: BTreeMap<String, FingerprintEntry>,
    visited: HashSet<String>,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl WorkspaceFingerprintBuilder {
    fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            visited: HashSet::new(),
            bytes: 0,
            max_entries,
            max_bytes,
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
            self.entries.insert(
                name,
                FingerprintEntry {
                    kind: b'f',
                    size,
                    permissions: permission_bits(&after),
                    content_hash: Some(content.finalize().into()),
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

pub(crate) fn workspace_fingerprint(run: &Run) -> Result<String> {
    workspace_fingerprint_with_limits(
        run,
        MAX_WORKSPACE_FINGERPRINT_ENTRIES,
        MAX_WORKSPACE_FINGERPRINT_BYTES,
    )
}

fn workspace_fingerprint_with_limits(
    run: &Run,
    max_entries: usize,
    max_bytes: u64,
) -> Result<String> {
    let workspace = Path::new(&run.workspace);
    let scopes = crate::filesystem::FileScopes::from_configuration(&run.budgets)?;
    let mut builder = WorkspaceFingerprintBuilder::new(max_entries, max_bytes);
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
    fn git_fingerprint_includes_tracked_and_unignored_scoped_files_but_skips_ignored_outputs(
    ) -> Result<()> {
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
        assert!(Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["init", "--quiet"])
            .status()?
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["add", "--", ".gitignore", "project/src/lib.rs"])
            .status()?
            .success());

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
        if previous.is_none() {
            let count: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM observed_files WHERE run_id=?1",
                [&operation.run_id],
                |row| row.get(0),
            )?;
            if count >= 1024 {
                bail!(
                    "File freshness ledger is bounded to 1024 observed paths; split the task before observing more"
                );
            }
        }
        transaction.execute("INSERT INTO observed_files(run_id,path,hash) VALUES (?1,?2,?3) ON CONFLICT(run_id,path) DO UPDATE SET hash=excluded.hash",params![operation.run_id,path,hash])?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn changed_observed_files(&self, id: &str) -> Result<Vec<(String, String, String)>> {
        let run = self.run(id)?;
        if run.is_terminal() {
            return Ok(Vec::new());
        }
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
                let file = File::open(target)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
                    bail!("Observed source is no longer a bounded regular file");
                }
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 2 * 1024 * 1024 {
                    bail!("Observed source grew beyond its bound");
                }
                Ok(hex::encode(Sha256::digest(&bytes)))
            })();
            let actual = digest.unwrap_or_else(|_| "unavailable".into());
            if actual != expected {
                changed.push((path, expected, actual));
            }
        }
        Ok(changed)
    }

    pub fn refresh_observed_files(&mut self, id: &str) -> Result<bool> {
        let workspace_changed = self.refresh_workspace_fingerprint(id)?;
        let changed = self.changed_observed_files(id)?;
        if changed.is_empty() {
            return Ok(workspace_changed);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut paths = Vec::new();
        for (path, expected, actual) in changed {
            let count = transaction.execute(
                "UPDATE observed_files SET hash=?4 WHERE run_id=?1 AND path=?2 AND hash=?3",
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
        workspace_fingerprint(&self.run(id)?)
    }

    pub(crate) fn workspace_fingerprint_baseline(&self, id: &str) -> Result<Option<(String, i64)>> {
        workspace_fingerprint_baseline(&self.connection, id)
    }

    pub(crate) fn refresh_workspace_fingerprint(&mut self, id: &str) -> Result<bool> {
        let baseline = self.workspace_fingerprint_baseline(id)?;
        if baseline.is_none() && !workspace_has_process_evidence(&self.connection, id)? {
            return Ok(false);
        }
        let fingerprint = self.capture_workspace_fingerprint(id)?;
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
                bail!("Existing process evidence predates scoped workspace fingerprinting; refresh the workspace and gather fresh evidence before verifying or completing");
            }
            return Ok(());
        };
        let run = self.run(id)?;
        let actual = workspace_fingerprint(&run)?;
        let revision = workspace_revision(&self.connection, id)?;
        if actual != expected {
            bail!("Scoped workspace content changed since process evidence was captured; run process.run again before verifying or completing");
        }
        if revision != baseline_revision {
            bail!("Scoped workspace fingerprint is not synchronized with the current workspace revision; refresh evidence before verifying or completing");
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
            "SELECT EXISTS(SELECT 1 FROM operations WHERE run_id=?1 AND capability='process.run' AND state='succeeded')",
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
