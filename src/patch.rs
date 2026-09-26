use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub old: String,
    pub new: String,
}

pub fn apply(original: &str, edits: &[Edit], expected: Option<&str>) -> Result<String> {
    if original.len() > 2 * 1024 * 1024 || edits.is_empty() || edits.len() > 32 {
        bail!("patch requires 1..32 edits and a file of at most 2 MiB");
    }
    if let Some(expected) = expected {
        if expected != hex::encode(Sha256::digest(original.as_bytes())) {
            bail!("file changed since the inspected SHA256; read it again before editing");
        }
    }
    let mut positions = Vec::new();
    let mut bytes = 0_usize;
    for edit in edits {
        bytes = bytes
            .saturating_add(edit.old.len())
            .saturating_add(edit.new.len());
        if edit.old.is_empty() || bytes > 64 * 1024 {
            bail!("patch text must be nonempty and total at most 64 KiB");
        }
        let start = original
            .find(&edit.old)
            .context("patch old text not found; read the file again")?;
        let next = start + edit.old.chars().next().unwrap().len_utf8();
        if original[next..].contains(&edit.old) {
            bail!("patch old text is ambiguous; include unique surrounding lines");
        }
        positions.push((start, start + edit.old.len(), &edit.new));
    }
    positions.sort_by_key(|(start, _, _)| *start);
    if positions.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        bail!("patch edits overlap; provide disjoint original ranges");
    }
    let mut output = String::new();
    let mut cursor = 0;
    for (start, end, replacement) in positions {
        output.push_str(&original[cursor..start]);
        output.push_str(replacement);
        cursor = end;
    }
    output.push_str(&original[cursor..]);
    if output.len() > 2 * 1024 * 1024 {
        bail!("patched file exceeds 2 MiB");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_use_unique_disjoint_original_ranges_and_guard_stale_content() -> Result<()> {
        let original = "alpha βeta gamma";
        let edits = [
            Edit {
                old: "alpha".into(),
                new: "gamma".into(),
            },
            Edit {
                old: "gamma".into(),
                new: "delta".into(),
            },
        ];
        assert_eq!(
            apply(
                original,
                &edits,
                Some(&hex::encode(Sha256::digest(original.as_bytes())))
            )?,
            "gamma βeta delta"
        );
        assert!(apply(original, &edits, Some("stale")).is_err());
        for (text, old) in [
            ("aaa", "aa"),
            ("βββ", "ββ"),
            ("abc abc", "abc"),
            ("text", "missing"),
            ("text", ""),
        ] {
            assert!(
                apply(
                    text,
                    &[Edit {
                        old: old.into(),
                        new: "changed".into()
                    }],
                    None
                )
                .is_err()
            );
        }
        assert!(
            apply(
                "abcde",
                &[
                    Edit {
                        old: "abc".into(),
                        new: "a".into()
                    },
                    Edit {
                        old: "cde".into(),
                        new: "b".into()
                    }
                ],
                None
            )
            .is_err()
        );
        assert_eq!(
            apply(
                "delete middle text",
                &[Edit {
                    old: "middle ".into(),
                    new: String::new()
                }],
                None
            )?,
            "delete text"
        );
        Ok(())
    }
}
