use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::{Color, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::storage::Event as RunEvent;

#[derive(Debug, PartialEq)]
pub enum Input {
    Submit(String),
    Providers,
    Sessions,
    Login,
    Exit,
}

#[derive(Clone, Copy)]
pub enum Tone {
    Accent,
    Quiet,
    Success,
    Warning,
}

pub struct Terminal {
    pub interactive: bool,
    colors: bool,
    animations: bool,
    frame: usize,
}

pub struct RawMode(bool);

impl RawMode {
    pub fn enter(enabled: bool) -> Result<Self> {
        if enabled {
            terminal::enable_raw_mode()?;
            let guard = Self(true);
            queue!(io::stdout(), event::EnableBracketedPaste)?;
            io::stdout().flush()?;
            return Ok(guard);
        }
        Ok(Self(enabled))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = terminal::disable_raw_mode();
            let _ = queue!(
                io::stdout(),
                event::DisableBracketedPaste,
                cursor::Show,
                ResetColor
            );
            let _ = io::stdout().flush();
        }
    }
}

pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .filter(
            |character| !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
        )
        .collect()
}

impl Default for Terminal {
    fn default() -> Self {
        let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
        Self {
            interactive,
            colors: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            animations: interactive && std::env::var_os("AEGIS_REDUCED_MOTION").is_none(),
            frame: 0,
        }
    }
}

impl Terminal {
    fn color(&self, tone: Tone) -> Color {
        match tone {
            Tone::Accent => Color::Rgb {
                r: 104,
                g: 211,
                b: 193,
            },
            Tone::Quiet => Color::DarkGrey,
            Tone::Success => Color::Rgb {
                r: 168,
                g: 211,
                b: 130,
            },
            Tone::Warning => Color::Rgb {
                r: 237,
                g: 185,
                b: 105,
            },
        }
    }

    pub fn message(&self, tone: Tone, label: &str, text: &str) -> Result<()> {
        let mut output = io::stdout();
        if self.colors {
            queue!(output, SetForegroundColor(self.color(tone)))?;
        }
        write!(output, "  {}", clean(label))?;
        if self.colors {
            queue!(output, ResetColor)?;
        }
        let text = clean(text).replace('\n', "\r\n    ");
        write!(output, "  {text}\r\n")?;
        output.flush()?;
        Ok(())
    }

    pub fn welcome(&self, provider: &str, workspace: &str) -> Result<()> {
        println!();
        self.message(Tone::Accent, "◈  A E G I S", "terminal agent runtime")?;
        self.message(
            Tone::Quiet,
            "─",
            &"─".repeat(
                terminal::size()
                    .map(|(width, _)| width.saturating_sub(6).min(64))
                    .unwrap_or(64) as usize,
            ),
        )?;
        self.message(Tone::Accent, "Provider", provider)?;
        self.message(Tone::Quiet, "Workspace", workspace)?;
        self.message(
            Tone::Quiet,
            "",
            "Describe the outcome. Aegis handles the steps.",
        )?;
        self.message(
            Tone::Quiet,
            "",
            "F2 provider · F3 sessions · F4 sign in · Ctrl+D exit",
        )?;
        println!();
        Ok(())
    }

    pub fn clear_activity(&self) -> Result<()> {
        if self.interactive {
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                cursor::Show
            )?;
            io::stdout().flush()?;
        }
        Ok(())
    }

    pub fn activity(&mut self, label: &str, elapsed: Duration, tokens: u64) -> Result<()> {
        if !self.interactive {
            return Ok(());
        }
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = if self.animations {
            frames[self.frame % frames.len()]
        } else {
            "◆"
        };
        self.frame += 1;
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80);
        let text = format!(
            "  {frame} {} · {}s · {} tokens  |  Ctrl+C stop · Ctrl+D detach",
            clean(label),
            elapsed.as_secs(),
            tokens
        );
        queue!(
            io::stdout(),
            cursor::Hide,
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        if self.colors {
            queue!(io::stdout(), SetForegroundColor(self.color(Tone::Accent)))?;
        }
        write!(io::stdout(), "{}", fit(&text, width.saturating_sub(1)))?;
        queue!(io::stdout(), ResetColor)?;
        io::stdout().flush()?;
        Ok(())
    }

    pub fn input(&self, label: &str, secret: bool, history: &[String]) -> Result<Input> {
        if !self.interactive {
            print!("{label}");
            io::stdout().flush()?;
            let mut text = String::new();
            if io::stdin().read_line(&mut text)? == 0 {
                return Ok(Input::Exit);
            }
            return Ok(Input::Submit(text.trim_end_matches(['\r', '\n']).into()));
        }
        let _raw = RawMode::enter(true)?;
        let mut text: Vec<char> = Vec::new();
        let mut caret = 0_usize;
        let mut history_index = history.len();
        loop {
            let width = terminal::size()
                .map(|(width, _)| width as usize)
                .unwrap_or(80);
            let label = fit(label, width.saturating_sub(2));
            let available = width.saturating_sub(label.width() + 1).max(1);
            let displayed: Vec<_> = text
                .iter()
                .map(|character| {
                    if secret {
                        '●'
                    } else if *character == '\n' {
                        '↵'
                    } else if character.is_control() {
                        ' '
                    } else {
                        *character
                    }
                })
                .collect();
            let (visible, caret_width) = input_view(&displayed, caret, available);
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine)
            )?;
            if self.colors {
                queue!(io::stdout(), SetForegroundColor(self.color(Tone::Accent)))?;
            }
            write!(io::stdout(), "{label}")?;
            queue!(io::stdout(), ResetColor)?;
            write!(io::stdout(), "{visible}")?;
            queue!(
                io::stdout(),
                cursor::MoveToColumn((label.width() + caret_width) as u16)
            )?;
            io::stdout().flush()?;
            match event::read()? {
                Event::Paste(paste) => {
                    let characters: Vec<_> = clean(&paste).chars().collect();
                    text.splice(caret..caret, characters.iter().copied());
                    caret += characters.len();
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Enter => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Submit(text.iter().collect()));
                        }
                        KeyCode::Char('d') if control && text.is_empty() => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Exit);
                        }
                        KeyCode::Char('c') if control => {
                            text.clear();
                            caret = 0;
                        }
                        KeyCode::Char('u') if control => {
                            text.clear();
                            caret = 0;
                        }
                        KeyCode::F(2) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Providers);
                        }
                        KeyCode::F(3) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Sessions);
                        }
                        KeyCode::F(4) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Login);
                        }
                        KeyCode::Left => caret = caret.saturating_sub(1),
                        KeyCode::Right => caret = (caret + 1).min(text.len()),
                        KeyCode::Home => caret = 0,
                        KeyCode::End => caret = text.len(),
                        KeyCode::Backspace if caret > 0 => {
                            caret -= 1;
                            text.remove(caret);
                        }
                        KeyCode::Delete if caret < text.len() => {
                            text.remove(caret);
                        }
                        KeyCode::Up if !secret && history_index > 0 => {
                            history_index -= 1;
                            text = history[history_index].chars().collect();
                            caret = text.len();
                        }
                        KeyCode::Down if !secret && history_index < history.len() => {
                            history_index += 1;
                            text = history
                                .get(history_index)
                                .map(|text| text.chars().collect())
                                .unwrap_or_default();
                            caret = text.len();
                        }
                        KeyCode::Char(character) if !control => {
                            text.insert(caret, character);
                            caret += 1;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    pub fn select(&self, title: &str, choices: &[String]) -> Result<Option<usize>> {
        if choices.is_empty() {
            return Ok(None);
        }
        self.message(Tone::Accent, "◇", title)?;
        if self.interactive {
            return self.menu(choices);
        }
        for (index, choice) in choices.iter().enumerate() {
            self.message(Tone::Quiet, &format!("{}", index + 1), choice)?;
        }
        loop {
            match self.input("  Choose › ", false, &[])? {
                Input::Submit(value) => {
                    if let Ok(index) = value.trim().parse::<usize>() {
                        if (1..=choices.len()).contains(&index) {
                            return Ok(Some(index - 1));
                        }
                    }
                    if value.trim().is_empty() {
                        return Ok(Some(0));
                    }
                }
                Input::Exit => return Ok(None),
                _ => {}
            }
        }
    }

    fn menu(&self, choices: &[String]) -> Result<Option<usize>> {
        let _raw = RawMode::enter(true)?;
        let rows = terminal::size()
            .map(|(_, height)| height.saturating_sub(5).max(1) as usize)
            .unwrap_or(8)
            .min(choices.len());
        let mut selected = 0_usize;
        let mut digits = String::new();
        let mut rendered = false;
        loop {
            let width = terminal::size()
                .map(|(width, _)| width as usize)
                .unwrap_or(80);
            if rendered {
                queue!(io::stdout(), cursor::MoveUp(rows as u16))?;
            }
            let start = selected.saturating_sub(rows / 2).min(choices.len() - rows);
            for (index, choice) in choices.iter().enumerate().skip(start).take(rows) {
                queue!(
                    io::stdout(),
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
                if self.colors {
                    queue!(
                        io::stdout(),
                        SetForegroundColor(self.color(if index == selected {
                            Tone::Accent
                        } else {
                            Tone::Quiet
                        }))
                    )?;
                }
                let marker = if index == selected { "›" } else { " " };
                write!(
                    io::stdout(),
                    "{}\r\n",
                    fit(
                        &format!("  {marker} {}  {choice}", index + 1),
                        width.saturating_sub(1)
                    )
                )?;
            }
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                ResetColor,
                cursor::Hide
            )?;
            write!(
                io::stdout(),
                "{}",
                fit(
                    "  ↑ ↓ choose · Enter confirm · Esc back",
                    width.saturating_sub(1)
                )
            )?;
            io::stdout().flush()?;
            rendered = true;
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => {
                        write!(io::stdout(), "\r\n")?;
                        return Ok(Some(selected));
                    }
                    KeyCode::Esc => {
                        write!(io::stdout(), "\r\n")?;
                        return Ok(None);
                    }
                    KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        write!(io::stdout(), "\r\n")?;
                        return Ok(None);
                    }
                    KeyCode::Char(digit) if digit.is_ascii_digit() => {
                        digits.push(digit);
                        if let Ok(number) = digits.parse::<usize>() {
                            if (1..=choices.len()).contains(&number) {
                                selected = number - 1;
                            } else {
                                digits.clear();
                            }
                        }
                    }
                    code => {
                        digits.clear();
                        selected = menu_move(selected, choices.len(), code);
                    }
                }
            }
        }
    }

    pub fn render_event(&self, event: &RunEvent) -> Result<()> {
        let payload = &event.payload;
        match event.kind.as_str() {
            "capability.search" => self.message(
                Tone::Accent,
                "Discover",
                payload["query"].as_str().unwrap_or_default(),
            ),
            "operation.pending" => {
                let args = &payload["arguments"];
                let detail = args["path"]
                    .as_str()
                    .or_else(|| args["query"].as_str())
                    .or_else(|| args["program"].as_str())
                    .unwrap_or_default();
                self.message(
                    Tone::Accent,
                    "Tool",
                    &format!(
                        "{}  {}",
                        payload["capability"].as_str().unwrap_or_default(),
                        fit(detail, 160)
                    ),
                )
            }
            "operation.succeeded" => self.message(Tone::Success, "✓", "Result stored as evidence"),
            "run.completed" => self.message(
                Tone::Success,
                "Done",
                payload["summary"].as_str().unwrap_or_default(),
            ),
            "checkpoint.created" => self.message(
                Tone::Quiet,
                "Checkpoint",
                payload["next_action"].as_str().unwrap_or_default(),
            ),
            "action.rejected" | "model.failed" => self.message(
                Tone::Warning,
                "!",
                &fit(payload["error"].as_str().unwrap_or("Action failed"), 500),
            ),
            "operation.failed" | "operation.outcome_unknown" => self.message(
                Tone::Warning,
                "!",
                payload["detail"]["error"]
                    .as_str()
                    .unwrap_or("Operation needs review"),
            ),
            "run.waiting_recovery" | "run.failed" => self.message(
                Tone::Warning,
                "Paused",
                payload["reason"]
                    .as_str()
                    .unwrap_or("Outcome needs reconciliation"),
            ),
            "run.cancelled" => self.message(Tone::Warning, "Stopped", "Task cancelled"),
            _ => Ok(()),
        }
    }
}

fn menu_move(selected: usize, count: usize, key: KeyCode) -> usize {
    match key {
        KeyCode::Up => (selected + count - 1) % count,
        KeyCode::Down => (selected + 1) % count,
        KeyCode::Home => 0,
        KeyCode::End => count - 1,
        _ => selected,
    }
}

fn input_view(text: &[char], caret: usize, width: usize) -> (String, usize) {
    let prefix_width: usize = text[..caret]
        .iter()
        .map(|character| character.width().unwrap_or(0))
        .sum();
    if prefix_width <= width {
        return (fit(&text.iter().collect::<String>(), width), prefix_width);
    }
    let mut start = caret;
    let mut caret_width = 0;
    while start > 0 {
        let size = text[start - 1].width().unwrap_or(0);
        if caret_width + size > width / 2 {
            break;
        }
        caret_width += size;
        start -= 1;
    }
    (
        fit(&text[start..].iter().collect::<String>(), width),
        caret_width,
    )
}

pub fn fit(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut occupied = 0;
    for character in clean(text).replace('\n', " ").chars() {
        let size = character.width().unwrap_or(0);
        if occupied + size > width {
            break;
        }
        occupied += size;
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_navigation_wraps_and_input_cursor_respects_wide_characters() {
        assert_eq!(menu_move(0, 4, KeyCode::Up), 3);
        assert_eq!(menu_move(3, 4, KeyCode::Down), 0);
        assert_eq!(menu_move(2, 4, KeyCode::Home), 0);
        assert_eq!(menu_move(0, 4, KeyCode::End), 3);
        let text: Vec<_> = "こんにちはhello".chars().collect();
        for caret in 0..=text.len() {
            let (visible, column) = input_view(&text, caret, 7);
            assert!(visible.width() <= 7);
            assert!(column <= 7);
        }
        assert_eq!(input_view(&['h', 'i'], 2, 20), ("hi".into(), 2));
    }

    #[test]
    fn clips_unicode_to_terminal_width_and_removes_control_sequences() {
        assert_eq!(fit("こんにちは", 6), "こんに");
        assert_eq!(fit("hello", 3), "hel");
        assert!(!clean("text\u{1b}[2J\u{202e}spoof").contains('\u{1b}'));
        assert!(!clean("text\u{202e}spoof").contains('\u{202e}'));
    }
}
