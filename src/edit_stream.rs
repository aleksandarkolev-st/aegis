//! Live, observational edit previews. Operation receipts remain execution proof.
use crate::{
    filesystem::{self, FileScopes},
    storage::{Operation, Run, Store},
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_DIFF_CHUNK: usize = 64 * 1024;

#[derive(Clone)]
struct File {
    modified: Option<SystemTime>,
    size: u64,
    text: Option<Arc<str>>,
}

pub(crate) struct Watch {
    run: Run,
    scopes: FileScopes,
    exact: Option<String>,
    files: BTreeMap<String, File>,
    next_poll: Instant,
}

impl Watch {
    pub(crate) fn start(run: &Run, operation: &Operation) -> Result<Option<Self>> {
        if !run
            .grants
            .as_array()
            .is_some_and(|grants| grants.iter().any(|v| v == "workspace.write"))
        {
            return Ok(None);
        }
        let exact = match operation.capability.as_str() {
            "workspace.write" | "workspace.patch" => Some(
                operation.arguments["path"]
                    .as_str()
                    .context("edit path missing")?
                    .to_owned(),
            ),
            "process.run" => None,
            name if name.starts_with("mcp.") => None,
            _ => return Ok(None),
        };
        let scopes = FileScopes::from_configuration(&run.budgets)?.unwrap_or(FileScopes {
            read: vec!["**".into()],
            write: vec!["**".into()],
        });
        let mut watch = Self {
            run: run.clone(),
            scopes,
            exact,
            files: BTreeMap::new(),
            next_poll: Instant::now(),
        };
        watch.files = watch.snapshot(true)?;
        Ok(Some(watch))
    }

    fn snapshot(&self, force: bool) -> Result<BTreeMap<String, File>> {
        let workspace = Path::new(&self.run.workspace);
        let mut pending = Vec::new();
        if let Some(path) = &self.exact {
            pending.push(self.scopes.checked_path(workspace, path, true)?);
        } else {
            for pattern in &self.scopes.write {
                let prefix = pattern.strip_suffix("/**").unwrap_or(pattern);
                pending.push(if prefix == "**" {
                    workspace.to_path_buf()
                } else {
                    self.scopes.checked_path(workspace, prefix, true)?
                });
            }
        }
        let mut files = BTreeMap::new();
        let mut visited = std::collections::HashSet::new();
        let mut bytes = 0;
        while let Some(path) = pending.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            if visited.len() > MAX_ENTRIES {
                bail!("live edit preview exceeds 10000 entries; narrow write scopes");
            }
            let relative = path
                .strip_prefix(workspace)?
                .to_string_lossy()
                .replace('\\', "/");
            if relative.split('/').any(filesystem::metadata_name) {
                continue;
            }
            // Do not traverse conventional dependency/build output trees.
            if self.exact.is_none()
                && relative.split('/').any(|part| {
                    matches!(
                        part,
                        "target" | "node_modules" | ".next" | "__pycache__" | ".venv"
                    )
                })
            {
                continue;
            }
            if self.run.budgets["remote_origin"] == true
                && filesystem::remote_secret_path(&relative)
            {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if filesystem::linked(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                for entry in fs::read_dir(&path)? {
                    if pending.len() + visited.len() >= MAX_ENTRIES {
                        bail!("live edit preview exceeds 10000 entries; narrow write scopes");
                    }
                    pending.push(entry?.path());
                }
                continue;
            }
            if !metadata.is_file() || !self.scopes.permits(&relative, true) {
                continue;
            }
            let path = self.scopes.checked_path(workspace, &relative, true)?;
            let modified = metadata.modified().ok();
            let size = metadata.len();
            if size > MAX_FILE || bytes + size as usize > MAX_SNAPSHOT {
                files.insert(
                    relative,
                    File {
                        modified,
                        size,
                        text: None,
                    },
                );
                continue;
            }
            bytes += size as usize;
            if !force
                && let Some(previous) = self.files.get(&relative)
                && modified.is_some()
                && previous.modified == modified
                && previous.size == size
            {
                files.insert(relative, previous.clone());
                continue;
            }
            let mut data = Vec::new();
            fs::File::open(path)?
                .take(MAX_FILE + 1)
                .read_to_end(&mut data)?;
            if data.len() as u64 > MAX_FILE {
                bail!("file grew beyond live edit preview limit");
            }
            let text = if data.contains(&0) {
                None
            } else {
                String::from_utf8(data).ok().map(Arc::from)
            };
            files.insert(
                relative,
                File {
                    modified,
                    size,
                    text,
                },
            );
        }
        Ok(files)
    }

    pub(crate) fn poll(
        &mut self,
        store: &mut Store,
        operation: &Operation,
        final_poll: bool,
    ) -> Result<()> {
        if !final_poll && Instant::now() < self.next_poll {
            return Ok(());
        }
        self.next_poll = Instant::now() + Duration::from_millis(300);
        let current = self.snapshot(final_poll)?;
        let paths: std::collections::BTreeSet<_> =
            self.files.keys().chain(current.keys()).cloned().collect();
        for path in paths {
            let before = self.files.get(&path);
            let after = current.get(&path);
            let old = before.and_then(|f| f.text.as_deref());
            let new = after.and_then(|f| f.text.as_deref());
            if old == new {
                continue;
            }
            // Binary transitions cannot truthfully be displayed as line edits.
            if before.is_some() && old.is_none() || after.is_some() && new.is_none() {
                continue;
            }
            let diff = unified(&path, old, new);
            if diff.is_empty() {
                continue;
            }
            let artifact = store.put_artifact(diff.as_bytes())?;
            let mut start = 0;
            while start < diff.len() {
                let mut end = start.saturating_add(MAX_DIFF_CHUNK).min(diff.len());
                while !diff.is_char_boundary(end) { end -= 1; }
                // Prefer complete lines; a single long line can span chunks.
                if end < diff.len() && let Some(newline) = diff[start..end].rfind('\n') {
                    end = start + newline + 1;
                }
                store.event(&operation.run_id, "operation.diff", json!({
                    "id":operation.id,"path":path,"text":&diff[start..end],"artifact":artifact,"truncated":false,
                    "offset":start,"total_bytes":diff.len(),"final_chunk":end==diff.len(),
                    "verification_scope":"Observed workspace change during this operation; not execution or completion proof"
                }))?;
                start = end;
            }
        }
        self.files = current;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    kind: char,
    text: &'a str,
}

// Bounded line matching: strip equal edges, then LCS small interiors. Large
// interiors use unique-line anchors to keep matching linear in file size.
fn changes<'a>(old: &[&'a str], new: &[&'a str], out: &mut Vec<Line<'a>>, depth: usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    out.extend(old[..prefix].iter().map(|text| Line { kind: ' ', text }));
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    if a.len().saturating_mul(b.len()) <= 262_144 && !a.is_empty() && !b.is_empty() {
        let width = b.len() + 1;
        let mut lengths = vec![0_u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lengths[i * width + j] = if a[i] == b[j] {
                    1 + lengths[(i + 1) * width + j + 1]
                } else {
                    lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                out.push(Line {
                    kind: ' ',
                    text: a[i],
                });
                i += 1;
                j += 1;
            } else if i < a.len()
                && (j == b.len() || lengths[(i + 1) * width + j] >= lengths[i * width + j + 1])
            {
                out.push(Line {
                    kind: '-',
                    text: a[i],
                });
                i += 1;
            } else {
                out.push(Line {
                    kind: '+',
                    text: b[j],
                });
                j += 1;
            }
        }
    } else {
        let mut positions = std::collections::HashMap::new();
        for (i, text) in a.iter().enumerate() {
            positions
                .entry(*text)
                .and_modify(|p: &mut Option<usize>| *p = None)
                .or_insert(Some(i));
        }
        let mut counts = std::collections::HashMap::new();
        for text in b {
            *counts.entry(*text).or_insert(0_usize) += 1;
        }
        let pairs: Vec<_> = b
            .iter()
            .enumerate()
            .filter_map(|(j, text)| {
                if counts[text] == 1 {
                    positions.get(text).copied().flatten().map(|i| (i, j))
                } else {
                    None
                }
            })
            .collect();
        let mut tails: Vec<usize> = Vec::new();
        let mut parents = vec![None; pairs.len()];
        for (index, (i, _)) in pairs.iter().enumerate() {
            let slot = tails.partition_point(|previous| pairs[*previous].0 < *i);
            if slot > 0 {
                parents[index] = Some(tails[slot - 1]);
            }
            if slot == tails.len() {
                tails.push(index);
            } else {
                tails[slot] = index;
            }
        }
        if depth < 32
            && let Some(mut index) = tails.last().copied()
        {
            let mut anchors = vec![pairs[index]];
            while let Some(parent) = parents[index] {
                index = parent;
                anchors.push(pairs[index]);
            }
            anchors.reverse();
            let (mut i, mut j) = (0, 0);
            for (ai, bj) in anchors {
                changes(&a[i..ai], &b[j..bj], out, depth + 1);
                out.push(Line {
                    kind: ' ',
                    text: a[ai],
                });
                i = ai + 1;
                j = bj + 1;
            }
            changes(&a[i..], &b[j..], out, depth + 1);
        } else {
            out.extend(a.iter().map(|text| Line { kind: '-', text }));
            out.extend(b.iter().map(|text| Line { kind: '+', text }));
        }
    }
    out.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|text| Line { kind: ' ', text }),
    );
}

fn unified(path: &str, old: Option<&str>, new: Option<&str>) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<_> = old.unwrap_or_default().split_inclusive('\n').collect();
    let b: Vec<_> = new.unwrap_or_default().split_inclusive('\n').collect();
    let mut lines = Vec::new();
    changes(&a, &b, &mut lines, 0);
    let old_path = old.map_or_else(|| "/dev/null".into(), |_| format!("a/{path}"));
    let new_path = new.map_or_else(|| "/dev/null".into(), |_| format!("b/{path}"));
    let mut output = format!("diff --git a/{path} b/{path}\n--- {old_path}\n+++ {new_path}\n");
    let changed: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != ' ')
        .map(|(i, _)| i)
        .collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for i in changed {
        let range = (i.saturating_sub(3), (i + 4).min(lines.len()));
        if let Some(last) = ranges.last_mut()
            && range.0 <= last.1
        {
            last.1 = range.1;
        } else {
            ranges.push(range);
        }
    }
    if ranges.is_empty() {
        return String::new();
    }
    let (mut old_line, mut new_line, mut cursor) = (1, 1, 0);
    for (start, end) in ranges {
        for line in &lines[cursor..start] {
            old_line += usize::from(line.kind != '+');
            new_line += usize::from(line.kind != '-');
        }
        let old_count = lines[start..end].iter().filter(|l| l.kind != '+').count();
        let new_count = lines[start..end].iter().filter(|l| l.kind != '-').count();
        output.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            if old_count == 0 {
                old_line - 1
            } else {
                old_line
            },
            old_count,
            if new_count == 0 {
                new_line - 1
            } else {
                new_line
            },
            new_count
        ));
        for line in &lines[start..end] {
            output.push(line.kind);
            output.push_str(line.text);
            if !line.text.ends_with('\n') {
                output.push_str("\n\\ No newline at end of file\n");
            }
            old_line += usize::from(line.kind != '+');
            new_line += usize::from(line.kind != '-');
        }
        cursor = end;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn additions_removals_repeated_lines_and_missing_newlines() {
        assert_eq!(
            unified("new.rs", None, Some("hello\n")),
            "diff --git a/new.rs b/new.rs\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+hello\n"
        );
        assert!(unified("old.rs", Some("bye\n"), None).contains("@@ -1,1 +0,0 @@\n-bye\n"));
        assert!(
            unified(
                "code.rs",
                Some("same\nold\nsame\n"),
                Some("same\nnew\nsame\n")
            )
            .contains(" same\n-old\n+new\n same\n")
        );
        assert!(
            unified("code.rs", Some("hello"), Some("hello\n"))
                .contains("-hello\n\\ No newline at end of file\n+hello\n")
        );
        assert_eq!(unified("same", Some("same"), Some("same")), "");
    }

    #[test]
    fn large_distant_changes_keep_hunks_small_and_reconstruct_both_inputs() {
        let original = (0..5000).map(|i| format!("line {i}\n")).collect::<String>();
        let updated = original
            .replace("line 25\n", "edited 25\n")
            .replace("line 4900\n", "edited 4900\n");
        let diff = unified("code.rs", Some(&original), Some(&updated));
        assert_eq!(diff.matches("@@ -").count(), 2);
        assert!(diff.len() < 1000);
        let a: Vec<_> = original.split_inclusive('\n').collect();
        let b: Vec<_> = updated.split_inclusive('\n').collect();
        let mut lines = Vec::new();
        changes(&a, &b, &mut lines, 0);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind != '+')
                .map(|l| l.text)
                .collect::<String>(),
            original
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.kind != '-')
                .map(|l| l.text)
                .collect::<String>(),
            updated
        );
    }

    #[test]
    fn watcher_keeps_scopes_metadata_binary_limits_and_noop_reads_out_of_diff_events() -> Result<()>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join(".arun");
        let mut store = Store::open(&root)?;
        fs::write(dir.path().join("code.rs"), "old\n")?;
        fs::write(dir.path().join("private.rs"), "PRIVATE_MARKER\n")?;
        fs::write(dir.path().join("binary.bin"), [0, 1, 2])?;
        fs::write(dir.path().join("big.bin"), vec![0; MAX_FILE as usize + 1])?;
        let run = store.create_run(
            "edit",
            dir.path(),
            "custom",
            json!(["workspace.write"]),
            json!({"filesystem_scopes":{"read":["code.rs"],"write":["code.rs"]}}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "process.run", json!({}), false)?;
        let mut watch = Watch::start(&run, &operation)?.unwrap();
        fs::write(dir.path().join("private.rs"), "PRIVATE_CHANGED\n")?;
        watch.poll(&mut store, &operation, true)?;
        assert_eq!(store.event_count(&run.id, "operation.diff")?, 0);
        fs::write(dir.path().join("code.rs"), "new\n")?;
        watch.poll(&mut store, &operation, true)?;
        let diff = store
            .events(&run.id)?
            .into_iter()
            .find(|e| e.kind == "operation.diff")
            .unwrap();
        assert!(
            diff.payload["text"]
                .as_str()
                .unwrap()
                .contains("-old\n+new\n")
        );
        assert!(
            store
                .recent_context_events(&run.id, 100)?
                .iter()
                .all(|e| e.kind != "operation.diff")
        );
        watch.poll(&mut store, &operation, true)?;
        assert_eq!(store.event_count(&run.id, "operation.diff")?, 1);
        let mut broad = run.clone();
        broad.budgets = json!({});
        let mut watch = Watch::start(&broad, &operation)?.unwrap();
        fs::write(root.join("hidden.rs"), "RUNTIME_SECRET\n")?;
        fs::write(dir.path().join("binary.bin"), [0, 3, 4])?;
        fs::write(dir.path().join("code.rs"), "again\n")?;
        watch.poll(&mut store, &operation, true)?;
        assert_eq!(store.event_count(&run.id, "operation.diff")?, 2);
        let events = serde_json::to_string(&store.events(&run.id)?)?;
        assert!(!events.contains("RUNTIME_SECRET"));
        assert!(!events.contains("PRIVATE_CHANGED"));
        Ok(())
    }

    #[test]
    fn preview_is_utf8_bounded_and_full_diff_artifact_survives() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::open(&dir.path().join(".arun"))?;
        let run = store.create_run(
            "write",
            dir.path(),
            "custom",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let op = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"code.rs","content":""}),
            false,
        )?;
        let mut watch = Watch::start(&run, &op)?.unwrap();
        fs::write(
            dir.path().join("code.rs"),
            format!("{}\nFINAL_MARKER\n", "🦀".repeat(20_000)),
        )?;
        watch.poll(&mut store, &op, true)?;
        let chunks: Vec<_> = store.events(&run.id)?.into_iter().filter(|e| e.kind == "operation.diff").collect();
        assert!(chunks.len() > 1);
        for event in &chunks {
            assert!(event.payload["text"].as_str().unwrap().len() <= MAX_DIFF_CHUNK);
            assert_eq!(event.payload["truncated"], false);
        }
        let full = String::from_utf8(store.artifact(chunks[0].payload["artifact"].as_str().unwrap())?)?;
        assert_eq!(chunks.iter().map(|e|e.payload["text"].as_str().unwrap()).collect::<String>(), full);
        assert!(full.contains("+FINAL_MARKER\n"));
        let through = chunks.last().unwrap().seq;
        // More than 128 subsequent changes must continue to arrive live.
        for index in 0..140 {
            fs::write(dir.path().join("code.rs"), format!("later edit {index} ??\n"))?;
            watch.poll(&mut store, &op, true)?;
        }
        let later: Vec<_> = store.events_since(&run.id, through)?.into_iter().filter(|e|e.kind == "operation.diff").collect();
        assert!(later.len() >= 140);
        assert!(later.last().unwrap().payload["text"].as_str().unwrap().contains("+later edit 139 ??\n"));
        Ok(())
    }
}
