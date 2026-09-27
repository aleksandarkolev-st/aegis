use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub struct Provider {
    pub binary: &'static str,
    pub package: &'static str,
    pub login: &'static [&'static str],
}

pub fn canonical(provider: &str) -> &str {
    match provider {
        "chatgpt" => "codex",
        "claude-code" => "claude",
        other => other,
    }
}

pub fn specification(provider: &str) -> Result<Provider> {
    match provider {
        "codex" | "chatgpt" => Ok(Provider {
            binary: "codex",
            package: "@openai/codex",
            login: &["login"],
        }),
        "claude" | "claude-code" => Ok(Provider {
            binary: "claude",
            package: "@anthropic-ai/claude-code",
            login: &["auth", "login"],
        }),
        "grok" => Ok(Provider {
            binary: "grok",
            package: "@xai-official/grok",
            login: &["login"],
        }),
        _ => bail!("native providers are ChatGPT, Claude Code, and Grok"),
    }
}

fn managed_home() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("AEGIS_PROVIDER_HOME") {
        return Ok(directory.into());
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("home directory unavailable; set AEGIS_PROVIDER_HOME")?;
    Ok(PathBuf::from(home).join(".aegis").join("providers"))
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

pub fn find(provider: &str) -> Result<Option<PathBuf>> {
    let info = specification(provider)?;
    if let Some(path) = system_executable(info.binary) {
        return Ok(Some(path));
    }
    let directory = managed_home()?
        .join(info.binary)
        .join("node_modules")
        .join(".bin");
    Ok(search(info.binary, [directory]))
}

pub fn executable(provider: &str) -> Result<PathBuf> {
    find(provider)?.with_context(|| {
        format!("{provider} CLI is not installed; open Aegis provider setup to install it")
    })
}

pub fn install(provider: &str) -> Result<()> {
    let info = specification(provider)?;
    let prefix = managed_home()?.join(info.binary);
    let directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let npm = search("npm", directories)
        .context("Node.js/npm is required for guided provider installation")?;
    let status = Command::new(npm)
        .args(["install", "--prefix"])
        .arg(&prefix)
        .args([
            "--no-audit",
            "--no-fund",
            "--save-exact",
            "--include=optional",
            info.package,
        ])
        .status()
        .context("start provider installation")?;
    if !status.success() {
        bail!("provider installation did not finish successfully")
    }
    if search(info.binary, [prefix.join("node_modules").join(".bin")]).is_none() {
        bail!("provider package installed without an executable for this platform");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_packages_and_aliases_are_fixed() -> Result<()> {
        assert_eq!(canonical("chatgpt"), "codex");
        assert_eq!(canonical("claude-code"), "claude");
        assert_eq!(canonical("fixture"), "fixture");
        assert_eq!(specification("chatgpt")?.package, "@openai/codex");
        assert_eq!(specification("claude-code")?.login, &["auth", "login"]);
        assert_eq!(specification("grok")?.package, "@xai-official/grok");
        assert!(specification("custom").is_err());
        assert!(specification("../../other").is_err());
        Ok(())
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
