use std::time::{Duration, Instant};

use anyhow::Result;
use arun::terminal::{ContextStatus, Terminal};
use arun::ui::{Palette, Phase, Skin, WelcomeBlock};

struct PocketComet;

impl Skin for PocketComet {
    fn palette(&self) -> Palette {
        Palette {
            accent: [255, 170, 112],
            ..Palette::default()
        }
    }
    fn portrait(&self) -> Vec<String> {
        ["   . *", "  (o_o)~~", "   ' *"].map(str::to_owned).into()
    }
    fn frame(&self, phase: Phase, tick: u64) -> String {
        if phase == Phase::Ready {
            return "(*_*)".into();
        }
        ["(o_o)~", "(-_o)~~", "(o_o)~~~"][tick as usize % 3].into()
    }
    fn blocks(&self) -> Vec<WelcomeBlock> {
        vec![WelcomeBlock::Mascot, WelcomeBlock::Hint]
    }
    fn input_prefix(&self) -> String {
        "comet > ".into()
    }
}

fn main() -> Result<()> {
    let mut terminal = Terminal::default().with_skin(PocketComet);
    terminal.welcome("Skin preview only", ".")?;
    let started = Instant::now();
    for _ in 0..30 {
        terminal.activity("Thinking", started.elapsed(), 0, &ContextStatus::default())?;
        std::thread::sleep(Duration::from_millis(100));
    }
    terminal.clear_activity()
}
