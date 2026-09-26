use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Palette {
    pub accent: [u8; 3],
    pub quiet: [u8; 3],
    pub success: [u8; 3],
    pub warning: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: [104, 211, 193],
            quiet: [139, 149, 163],
            success: [168, 211, 130],
            warning: [237, 185, 105],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WelcomeBlock {
    Mascot,
    Provider,
    Workspace,
    Hint,
    Shortcuts,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mascot {
    #[default]
    Pip,
    Byte,
    Orbit,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    Working,
    Ready,
}

pub trait Skin {
    fn palette(&self) -> Palette;
    fn portrait(&self) -> Vec<String>;
    fn frame(&self, phase: Phase, tick: u64) -> String;
    fn blocks(&self) -> Vec<WelcomeBlock> {
        UiOptions::default().blocks
    }
    fn frame_interval(&self) -> Duration {
        Duration::from_millis(400)
    }
    fn input_prefix(&self) -> String {
        "  › ".into()
    }
    fn show_context(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiOptions {
    pub palette: Palette,
    pub mascot: Mascot,
    pub motion: bool,
    pub frame_millis: u64,
    pub blocks: Vec<WelcomeBlock>,
    pub frames: Vec<String>,
    pub portrait: Vec<String>,
    pub input_prefix: String,
    pub show_context: bool,
}

impl Default for UiOptions {
    fn default() -> Self {
        Self {
            palette: Palette::default(),
            mascot: Mascot::Pip,
            motion: true,
            frame_millis: 400,
            blocks: vec![
                WelcomeBlock::Mascot,
                WelcomeBlock::Provider,
                WelcomeBlock::Workspace,
                WelcomeBlock::Hint,
                WelcomeBlock::Shortcuts,
            ],
            frames: Vec::new(),
            portrait: Vec::new(),
            input_prefix: "  › ".into(),
            show_context: true,
        }
    }
}

impl UiOptions {
    pub fn preset(index: usize) -> Self {
        match index {
            1 => Self {
                mascot: Mascot::Byte,
                palette: Palette {
                    accent: [183, 157, 255],
                    quiet: [143, 151, 173],
                    success: [119, 219, 188],
                    warning: [255, 189, 126],
                },
                ..Self::default()
            },
            2 => Self {
                mascot: Mascot::Orbit,
                palette: Palette {
                    accent: [255, 193, 105],
                    quiet: [162, 153, 140],
                    success: [172, 215, 146],
                    warning: [255, 141, 133],
                },
                ..Self::default()
            },
            3 => Self {
                mascot: Mascot::Off,
                motion: false,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(80..=2000).contains(&self.frame_millis) || self.blocks.len() > 5 {
            bail!("style needs 80..2000 milliseconds per frame and at most five welcome blocks");
        }
        for (index, block) in self.blocks.iter().enumerate() {
            if self.blocks[..index].contains(block) {
                bail!("welcome blocks must not repeat");
            }
        }
        if self.frames.len() > 32 || self.portrait.len() > 8 {
            bail!("style supports at most 32 frames and eight portrait rows");
        }
        for text in self
            .frames
            .iter()
            .chain(&self.portrait)
            .chain(std::iter::once(&self.input_prefix))
        {
            if text.width() > 64
                || text.len() > 256
                || text != &crate::terminal::clean(text)
                || text.contains('\n')
            {
                bail!("style text must be one safe line, at most 64 columns and 256 bytes");
            }
        }
        if self.input_prefix.width() > 16 {
            bail!("input prefix must fit in 16 columns");
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(32 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 32 * 1024 {
            bail!("style file exceeds 32 KiB");
        }
        let options: Self = serde_json::from_slice(&bytes).context("invalid UI style")?;
        options.validate()?;
        Ok(options)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        let mut temporary = tempfile::NamedTempFile::new_in(root)?;
        temporary.write_all(&serde_json::to_vec_pretty(self)?)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(root.join("ui.json"))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

impl Skin for UiOptions {
    fn palette(&self) -> Palette {
        self.palette.clone()
    }
    fn portrait(&self) -> Vec<String> {
        if self.mascot == Mascot::Off {
            return Vec::new();
        }
        if !self.portrait.is_empty() {
            return self.portrait.clone();
        }
        let rows = match self.mascot {
            Mascot::Pip => ["   .-----.", "   | o o |", "   \\  ^  /", "    '---'"],
            Mascot::Byte => ["    .-.-.", "   [ o o ]", "   |  =  |", "    '-.-'"],
            Mascot::Orbit => ["     . + .", "   --(o)--", "     ' + '", "      *"],
            Mascot::Off => unreachable!(),
        };
        rows.map(str::to_owned).into()
    }
    fn frame(&self, phase: Phase, tick: u64) -> String {
        if self.mascot == Mascot::Off {
            return String::new();
        }
        let tick = if self.motion { tick } else { 0 };
        if !self.frames.is_empty() {
            return self.frames[tick as usize % self.frames.len()].clone();
        }
        if phase == Phase::Ready {
            return "<^.^>".into();
        }
        let frames = match (self.mascot, phase) {
            (Mascot::Pip, Phase::Thinking) => ["<o.o>", "<o.o>", "<o.->", "<o.o>"],
            (Mascot::Pip, _) => ["<o.o>", "<^.^>", "<o.o>", "<^.^>"],
            (Mascot::Byte, _) => ["[o.o]", "[o.O]", "[O.o]", "[o.o]"],
            (Mascot::Orbit, _) => ["-o-", "\\o/", "|o|", "/o\\"],
            (Mascot::Off, _) => unreachable!(),
        };
        frames[tick as usize % frames.len()].into()
    }
    fn blocks(&self) -> Vec<WelcomeBlock> {
        self.blocks.clone()
    }
    fn frame_interval(&self) -> Duration {
        Duration::from_millis(self.frame_millis)
    }
    fn input_prefix(&self) -> String {
        self.input_prefix.clone()
    }
    fn show_context(&self) -> bool {
        self.show_context
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mascot_animates_and_quiet_style_is_static() {
        let style = UiOptions::default();
        assert_ne!(
            style.frame(Phase::Thinking, 0),
            style.frame(Phase::Thinking, 2)
        );
        assert_eq!(style.frame(Phase::Ready, 0), "<^.^>");
        assert!(!style.portrait().is_empty());
        let calm = UiOptions {
            motion: false,
            ..style
        };
        assert_eq!(
            calm.frame(Phase::Thinking, 0),
            calm.frame(Phase::Thinking, 2)
        );
        assert!(UiOptions::preset(3).frame(Phase::Working, 2).is_empty());
    }

    #[test]
    fn styles_are_optional_bounded_composable_data_not_executable_plugins() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ui.json");
        std::fs::write(
            &path,
            r#"{"frames":["(*_*)","(-_-)"] ,"blocks":["mascot","hint"],"input_prefix":"build > "}"#,
        )?;
        let style = UiOptions::from_file(&path)?;
        assert_eq!(style.frame(Phase::Thinking, 1), "(-_-)");
        assert_eq!(
            style.blocks(),
            vec![WelcomeBlock::Mascot, WelcomeBlock::Hint]
        );
        style.save(directory.path())?;
        assert_eq!(UiOptions::from_file(&path)?.input_prefix, "build > ");
        for invalid in [
            r#"{"frames":["\u001b[2J"]}"#,
            r#"{"frames":["\u202espoof"]}"#,
            r#"{"blocks":["hint","hint"]}"#,
            r#"{"frame_millis":0}"#,
            r#"{"script":"run arbitrary code"}"#,
        ] {
            std::fs::write(&path, invalid)?;
            assert!(UiOptions::from_file(&path).is_err());
        }
        std::fs::write(&path, vec![b' '; 32769])?;
        assert!(UiOptions::from_file(&path).is_err());
        Ok(())
    }
}
