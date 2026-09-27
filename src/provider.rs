use std::path::{Path, PathBuf};

pub fn canonical(provider: &str) -> &str {
    match provider {
        "chatgpt" => "codex",
        "claude-code" => "claude",
        other => other,
    }
}

fn search(binary: &str, directories: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd"]
    } else {
        &[""]
    };
    for directory in directories {
        for suffix in suffixes {
            let candidate = directory.join(format!("{binary}{suffix}"));
            if executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn system_executable(binary: &str) -> Option<PathBuf> {
    let directories: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    search(binary, directories)
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[test]
    fn provider_aliases_are_fixed_without_native_packages() {
        assert_eq!(canonical("chatgpt"), "codex");
        assert_eq!(canonical("claude-code"), "claude");
        assert_eq!(canonical("fixture"), "fixture");
        assert_eq!(canonical("grok"), "grok");
        assert_eq!(canonical("custom"), "custom");
    }

    #[test]
    fn resolver_prefers_a_native_binary_and_ignores_non_executable_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let filename = if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        };
        let path = directory.path().join(filename);
        std::fs::write(&path, "fixture")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(search("fixture", [directory.path().to_owned()]).is_none());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(windows)]
        std::fs::write(directory.path().join("fixture.cmd"), "wrapper")?;
        assert_eq!(search("fixture", [directory.path().to_owned()]), Some(path));
        assert!(search("missing", [directory.path().to_owned()]).is_none());
        Ok(())
    }
}
