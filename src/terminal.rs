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
pub use crate::text::clean;

#[derive(Debug, PartialEq)]
pub enum Input {
    Submit(String),
    Providers,
    Models,
    Settings,
    Help,
    Checkpoint,
    CancelTask,
    Sessions,
    Login,
    NewConversation,
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
    activity_rows: std::cell::Cell<u16>,
    activity_view: std::cell::RefCell<Option<(usize, String, Option<String>)>>,
    input_draft: std::cell::RefCell<Option<(String, Vec<char>, usize)>>,
    skin: Box<dyn crate::ui::Skin>,
}

#[derive(Default)]
pub struct ContextStatus {
    pub prompt_chars: Option<u64>,
    pub normalized_tokens: Option<u64>,
    pub schema_count: Option<u64>,
    pub operations: u64,
    pub artifacts: usize,
    pub checkpoint_at: Option<i64>,
}

impl ContextStatus {
    pub fn text(&self, now: i64) -> String {
        let prompt = self
            .prompt_chars
            .map(|chars| chars.to_string())
            .unwrap_or_else(|| "?".into());
        let schemas = self
            .schema_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "?".into());
        let checkpoint = self
            .checkpoint_at
            .map(|created| format!("{}s", now.saturating_sub(created).max(0)))
            .unwrap_or_else(|| "—".into());
        let context = self
            .normalized_tokens
            .map(|tokens| format!("ctx {tokens} o200k units"))
            .unwrap_or_else(|| format!("ctx {prompt} chars"));
        format!(
            "{context} · schemas {schemas} · ops {} · artifacts {} · checkpoint {checkpoint}",
            self.operations, self.artifacts
        )
    }
}

pub struct RawMode(bool);

impl RawMode {
    pub fn enter(enabled: bool) -> Result<Self> {
        if enabled {
            if terminal::is_raw_mode_enabled()? {
                return Ok(Self(false));
            }
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

pub fn friendly_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if [
        "401",
        "token has expired",
        "not authenticated",
        "not logged in",
        "please log in",
        "authentication_error",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return "Your provider sign-in has expired or is missing. Press F4 to sign in, then continue the saved task from F3.".into();
    }
    if [
        "429",
        "rate limit",
        "usage limit",
        "quota",
        "too many requests",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return "The provider's usage limit was reached. Wait for it to reset, or choose another provider/model with F2 or F6. Your task is saved.".into();
    }
    if [
        "model_not_found",
        "model not found",
        "model is not available",
        "unsupported model",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return "This model isn't available for your account. Press F6 to choose another model."
            .into();
    }
    if ["timed out", "timeout", "deadline"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return "The operation exceeded its time limit. Your task is saved; review its outcome in F3 before continuing.".into();
    }
    if lower.contains("context")
        && ["exceed", "too long", "overflow", "limit"]
            .iter()
            .any(|needle| lower.contains(needle))
    {
        return "The provider's context limit was reached. Open F3 to review the saved task and its context.".into();
    }
    if error.contains('{')
        || error.contains('[')
        || lower.contains("bearer ")
        || lower.contains("ghp_")
        || lower.contains("sk-")
    {
        return "The provider or tool returned an error. Your task is saved; open F3 for recovery and use the trace view for diagnostics.".into();
    }
    fit(error, 240)
}

impl Default for Terminal {
    fn default() -> Self {
        let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
        Self {
            interactive,
            colors: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            animations: interactive && std::env::var_os("AEGIS_REDUCED_MOTION").is_none(),
            activity_rows: std::cell::Cell::new(0),
            activity_view: std::cell::RefCell::new(None),
            input_draft: std::cell::RefCell::new(None),
            skin: Box::new(crate::ui::UiOptions::default()),
        }
    }
}

impl Terminal {
    fn color(&self, tone: Tone) -> Color {
        let palette = self.skin.palette();
        let [r, g, b] = match tone {
            Tone::Accent => palette.accent,
            Tone::Quiet => palette.quiet,
            Tone::Success => palette.success,
            Tone::Warning => palette.warning,
        };
        Color::Rgb { r, g, b }
    }

    fn set_skin(&mut self, skin: Box<dyn crate::ui::Skin>) {
        let previous_prefix = self.input_prefix();
        self.activity_view.replace(None);
        self.skin = skin;
        let next_prefix = self.input_prefix();
        if let Some((label, _, _)) = self.input_draft.borrow_mut().as_mut() {
            if *label == previous_prefix {
                *label = next_prefix;
            }
        }
    }

    pub fn with_skin(mut self, skin: impl crate::ui::Skin + 'static) -> Self {
        self.set_skin(Box::new(skin));
        self
    }

    pub fn apply_ui(&mut self, options: crate::ui::UiOptions) -> Result<()> {
        self.activity_view.replace(None);
        options.validate()?;
        if self.activity_rows.get() > 0 {
            self.clear_activity()?;
        }
        self.animations = self.interactive
            && options.motion
            && std::env::var_os("AEGIS_REDUCED_MOTION").is_none();
        self.colors =
            io::stdout().is_terminal() && options.colors && std::env::var_os("NO_COLOR").is_none();
        self.set_skin(Box::new(options));
        Ok(())
    }

    pub fn load_ui(&mut self, root: &std::path::Path) -> Result<()> {
        let path = root.join("ui.json");
        if !path.exists() {
            return Ok(());
        }
        match crate::ui::UiOptions::from_file(&path) {
            Ok(style) => self.apply_ui(style)?,
            Err(error) => self.message(
                Tone::Quiet,
                "Style fallback",
                &format!(
                    "Couldn't load your style; defaults are ready. {}",
                    fit(&error.to_string(), 160)
                ),
            )?,
        }
        Ok(())
    }

    pub fn input_prefix(&self) -> String {
        fit(&self.skin.input_prefix(), 16)
    }

    pub fn message(&self, tone: Tone, label: &str, text: &str) -> Result<()> {
        let mut output = io::stdout();
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80)
            .saturating_sub(1);
        let label = if self.interactive {
            fit(label, width / 2)
        } else {
            clean(label)
        };
        if self.colors {
            queue!(output, SetForegroundColor(self.color(tone)))?;
        }
        write!(output, "  {label}")?;
        if self.colors {
            queue!(output, ResetColor)?;
        }
        let text = if matches!(tone, Tone::Warning) {
            friendly_error(text)
        } else {
            clean(text)
        };
        let text = if self.interactive {
            wrap_text(
                &text,
                width.saturating_sub(label.width() + 4),
                width.saturating_sub(4),
            )
            .join("\r\n    ")
        } else {
            text.replace('\n', "\r\n    ")
        };
        write!(output, "  {text}\r\n")?;
        output.flush()?;
        Ok(())
    }

    pub fn welcome(&self, provider: &str, workspace: &str) -> Result<()> {
        println!();
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80)
            .saturating_sub(4);
        for block in self.skin.blocks().into_iter().take(5) {
            match block {
                crate::ui::WelcomeBlock::Mascot => {
                    let portrait = self.skin.portrait();
                    if portrait.is_empty() {
                        self.message(Tone::Accent, "A E G I S", "Your terminal sidekick.")?;
                    } else {
                        for (index, row) in portrait.iter().take(8).enumerate() {
                            let caption = [
                                "A E G I S",
                                "Your terminal sidekick.",
                                "Big ideas. Tiny footprint.",
                            ]
                            .get(index)
                            .copied()
                            .unwrap_or("");
                            self.message(
                                Tone::Accent,
                                "",
                                &fit(&format!("{row:<15} {caption}"), width),
                            )?;
                        }
                    }
                }
                crate::ui::WelcomeBlock::Provider => self.message(
                    Tone::Accent,
                    "Provider",
                    &fit(provider, width.saturating_sub(10)),
                )?,
                crate::ui::WelcomeBlock::Workspace => self.message(
                    Tone::Quiet,
                    "Workspace",
                    &fit(workspace, width.saturating_sub(11)),
                )?,
                crate::ui::WelcomeBlock::Hint => {
                    self.message(Tone::Quiet, "", "What would you like to build? Just ask.")?
                }
                crate::ui::WelcomeBlock::Shortcuts => {
                    self.message(
                        Tone::Quiet,
                        "",
                        &fit("F2 provider · F6 models · F7 personalize · F3 tasks", width),
                    )?;
                    self.message(
                        Tone::Quiet,
                        "",
                        &fit("F1 help · F4 sign in · F5 fresh start · Ctrl+D exit", width),
                    )?;
                }
            }
        }
        println!();
        Ok(())
    }

    pub fn clear_activity(&self) -> Result<()> {
        self.activity_view.replace(None);
        if self.interactive {
            let rows = self.activity_rows.replace(0);
            if rows > 1 {
                queue!(io::stdout(), cursor::MoveUp(rows - 1))?;
            }
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                cursor::Show
            )?;
            for _ in 1..rows {
                queue!(
                    io::stdout(),
                    cursor::MoveDown(1),
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
            }
            if rows > 1 {
                queue!(
                    io::stdout(),
                    cursor::MoveUp(rows - 1),
                    cursor::MoveToColumn(0)
                )?;
            }
            io::stdout().flush()?;
        }
        Ok(())
    }

    pub fn activity(
        &mut self,
        label: &str,
        elapsed: Duration,
        tokens: u64,
        context: &ContextStatus,
    ) -> Result<()> {
        if !self.interactive {
            return Ok(());
        }
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let phase = if label.contains("Thinking") {
            crate::ui::Phase::Thinking
        } else {
            crate::ui::Phase::Working
        };
        let interval = self.skin.frame_interval().as_millis().max(1);
        let tick = if self.animations {
            (elapsed.as_millis() / interval) as u64
        } else {
            0
        };
        let mascot = fit(&self.skin.frame(phase, tick), 24);
        let frame = if !mascot.is_empty() {
            mascot.as_str()
        } else if self.animations {
            frames[tick as usize % frames.len()]
        } else {
            "◆"
        };
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80);
        let text = format!(
            "  {frame} {} · {}s · {} recorded tokens · ^C interrupt / ^D detach",
            fit(label, 20),
            elapsed.as_secs(),
            tokens
        );
        let text = fit(&text, width.saturating_sub(1));
        let footer = self.skin.show_context().then(|| {
            fit(
                &format!("  {}", context.text(crate::storage::unix_time())),
                width.saturating_sub(1),
            )
        });
        let view = (width, text, footer);
        if self.activity_view.borrow().as_ref() == Some(&view) {
            return Ok(());
        }
        let rows = self.activity_rows.get();
        if rows > 1 {
            queue!(io::stdout(), cursor::MoveUp(rows - 1))?;
        }
        queue!(
            io::stdout(),
            cursor::Hide,
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        if self.colors {
            queue!(io::stdout(), SetForegroundColor(self.color(Tone::Accent)))?;
        }
        write!(io::stdout(), "{}", view.1)?;
        queue!(io::stdout(), ResetColor)?;
        if !self.skin.show_context() {
            self.activity_rows.set(1);
            io::stdout().flush()?;
            self.activity_view.replace(Some(view));
            return Ok(());
        }
        write!(io::stdout(), "\r\n")?;
        queue!(
            io::stdout(),
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        write!(io::stdout(), "{}", view.2.as_deref().unwrap_or_default())?;
        self.activity_rows.set(2);
        io::stdout().flush()?;
        self.activity_view.replace(Some(view));
        Ok(())
    }

    fn take_input_draft(&self, label: &str, secret: bool) -> (Vec<char>, usize) {
        let mut draft = self.input_draft.borrow_mut();
        if !secret && draft.as_ref().is_some_and(|saved| saved.0 == label) {
            let (_, text, caret) = draft.take().unwrap();
            let caret = caret.min(text.len());
            return (text, caret);
        }
        (Vec::new(), 0)
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
        let draft_label = label.to_owned();
        let (mut text, mut caret) = self.take_input_draft(label, secret);
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
                    if !secret && matches!(key.code, KeyCode::F(1..=9)) {
                        self.input_draft
                            .replace(Some((draft_label.clone(), text.clone(), caret)));
                    }
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
                        KeyCode::F(5) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::NewConversation);
                        }
                        KeyCode::F(6) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Models);
                        }
                        KeyCode::F(7) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Settings);
                        }
                        KeyCode::F(1) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Help);
                        }
                        KeyCode::F(8) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::Checkpoint);
                        }
                        KeyCode::F(9) if !secret => {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Input::CancelTask);
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
        let mut query = String::new();
        let mut rendered = false;
        loop {
            let width = terminal::size()
                .map(|(width, _)| width as usize)
                .unwrap_or(80);
            if rendered {
                queue!(io::stdout(), cursor::MoveUp(rows as u16))?;
            }
            let matches = menu_matches(choices, &query);
            selected = selected.min(matches.len().saturating_sub(1));
            let start = selected
                .saturating_sub(rows / 2)
                .min(matches.len().saturating_sub(rows));
            for row in 0..rows {
                let position = start + row;
                let index = matches.get(position).copied();
                let choice = index
                    .map(|index| choices[index].as_str())
                    .unwrap_or(if row == 0 {
                        "No matching options · Backspace clears the filter"
                    } else {
                        ""
                    });
                queue!(
                    io::stdout(),
                    cursor::MoveToColumn(0),
                    Clear(ClearType::CurrentLine)
                )?;
                if self.colors {
                    queue!(
                        io::stdout(),
                        SetForegroundColor(self.color(
                            if index.is_some() && position == selected {
                                Tone::Accent
                            } else {
                                Tone::Quiet
                            }
                        ))
                    )?;
                }
                let marker = if index.is_some() && position == selected {
                    "›"
                } else {
                    " "
                };
                let number = index
                    .map(|index| (index + 1).to_string())
                    .unwrap_or_default();
                write!(
                    io::stdout(),
                    "{}\r\n",
                    fit(
                        &format!("  {marker} {number}  {choice}"),
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
                    &format!("  ↑ ↓ choose · Enter confirm · Esc back · Filter: {query}"),
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
                        if let Some(index) = matches.get(selected) {
                            write!(io::stdout(), "\r\n")?;
                            return Ok(Some(*index));
                        }
                    }
                    KeyCode::Esc => {
                        write!(io::stdout(), "\r\n")?;
                        return Ok(None);
                    }
                    KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        write!(io::stdout(), "\r\n")?;
                        return Ok(None);
                    }
                    KeyCode::Char(digit) if digit.is_ascii_digit() && query.is_empty() => {
                        digits.push(digit);
                        if let Ok(number) = digits.parse::<usize>() {
                            if (1..=choices.len()).contains(&number) {
                                selected = number - 1;
                            } else {
                                digits.clear();
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        digits.clear();
                        selected = 0;
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if query.len() < 160 {
                            query.push(character);
                        }
                        digits.clear();
                        selected = 0;
                    }
                    code => {
                        digits.clear();
                        if !matches.is_empty() {
                            selected = menu_move(selected, matches.len(), code);
                        }
                    }
                }
            }
        }
    }

    pub fn render_event(&self, event: &RunEvent) -> Result<()> {
        let payload = &event.payload;
        match event.kind.as_str() {
            "acceptance.started" => self.message(
                Tone::Accent,
                "Verify",
                payload["name"]
                    .as_str()
                    .unwrap_or("Independent completion check"),
            ),
            "acceptance.passed" => self.message(
                Tone::Success,
                "Verified",
                payload["name"].as_str().unwrap_or("Acceptance passed"),
            ),
            "acceptance.failed" => self.message(
                Tone::Warning,
                "Check failed",
                &fit(
                    payload["excerpt"]
                        .as_str()
                        .unwrap_or("Task remains unfinished"),
                    500,
                ),
            ),
            "acceptance.unavailable" => self.message(
                Tone::Warning,
                "Check unavailable",
                "Task stays paused until its verification environment is ready.",
            ),
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
                    .or_else(|| args["url"].as_str())
                    .unwrap_or_default();
                let capability = payload["capability"].as_str().unwrap_or("tool");
                let label = match capability {
                    "workspace.read" => "Read",
                    "workspace.write" | "workspace.patch" => "Edit",
                    "workspace.search" => "Search",
                    "process.run" => "Run",
                    "network.fetch" => "Fetch",
                    _ => "Tool",
                };
                self.message(
                    Tone::Accent,
                    label,
                    &format!(
                        "{}  {}",
                        if label == "Tool" { capability } else { "" },
                        fit(detail, 160)
                    ),
                )
            }
            "operation.succeeded" => {
                let (tone, label, text) = result_summary(payload);
                self.message(tone, label, &text)
            }
            "run.answered" => self.message(
                Tone::Accent,
                "Aegis",
                payload["summary"].as_str().unwrap_or_default(),
            ),
            "run.completed" => {
                let face = fit(&self.skin.frame(crate::ui::Phase::Ready, 0), 24);
                let label = if face.is_empty() {
                    "Done".into()
                } else {
                    format!("{face} Done")
                };
                self.message(
                    Tone::Success,
                    &label,
                    payload["summary"].as_str().unwrap_or_default(),
                )
            }
            "checkpoint.created" => self.message(
                Tone::Quiet,
                "Checkpoint",
                payload["next_action"].as_str().unwrap_or_default(),
            ),
            "action.rejected" | "model.failed" => self.message(
                Tone::Warning,
                "!",
                &friendly_error(payload["error"].as_str().unwrap_or("Action failed")),
            ),
            "operation.cancelled" => self.message(
                Tone::Warning,
                "Interrupted",
                "Operation stopped; no unsafe effects were assumed undone.",
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

fn menu_matches(choices: &[String], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    choices
        .iter()
        .enumerate()
        .filter(|(_, choice)| choice.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}

fn result_summary(payload: &serde_json::Value) -> (Tone, &'static str, String) {
    let detail = &payload["detail"];
    let capability = detail["capability"].as_str().unwrap_or("tool");
    let target = detail["target"].as_str().unwrap_or(capability);
    let mut text = fit(target, 100);
    let code = detail["exit_code"].as_i64();
    if let Some(code) = code {
        text.push_str(&format!(" · exit {code}"));
    }
    if let Some(matches) = detail["matches"].as_u64() {
        text.push_str(&format!(" · {matches} matches"));
    }
    if let Some(bytes) = detail["output_bytes"].as_u64() {
        let size = if bytes >= 1024 * 1024 {
            format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{:.1} KiB", bytes as f64 / 1024.0)
        } else {
            format!("{bytes} B")
        };
        text.push_str(&format!(" · {size}"));
    }
    if let Some(elapsed) = detail["elapsed_ms"].as_u64() {
        text.push_str(&format!(" · {:.1}s", elapsed as f64 / 1000.0));
    }
    if let Some(hash) = detail["output_artifact"]
        .as_str()
        .or_else(|| payload["artifact"].as_str())
    {
        text.push_str(&format!(" · evidence {}", fit(hash, 12)));
    } else {
        text.push_str(" · evidence saved");
    }
    if capability == "process.run" && code.is_none() {
        return (
            Tone::Warning,
            "Command ended",
            format!("{text} · exit status unavailable"),
        );
    }
    if code.is_some_and(|code| code != 0) {
        return (Tone::Warning, "Command failed", text);
    }
    let label = match capability {
        "workspace.write" | "workspace.patch" => "Edited",
        "workspace.read" => "Read",
        "workspace.search" => "Found",
        "process.run" => "Command finished",
        _ => "Tool finished",
    };
    (Tone::Success, label, text)
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

fn wrap_text(text: &str, first_width: usize, next_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut remaining = paragraph.trim_end();
        if remaining.is_empty() {
            lines.push(String::new());
        }
        while !remaining.is_empty() {
            let width = if lines.is_empty() {
                first_width
            } else {
                next_width
            }
            .max(1);
            let fitted = fit(remaining, width);
            if fitted.is_empty() {
                let character = remaining.chars().next().unwrap();
                lines.push("?".into());
                remaining = &remaining[character.len_utf8()..];
                continue;
            }
            let cut = if fitted.len() < remaining.len() {
                fitted
                    .rfind(char::is_whitespace)
                    .filter(|cut| *cut > 0)
                    .unwrap_or(fitted.len())
            } else {
                fitted.len()
            };
            lines.push(remaining[..cut].trim_end().to_owned());
            remaining = remaining[cut..].trim_start();
        }
    }
    lines
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
    fn activity_cache_is_invalidated_after_clear_and_style_changes() -> Result<()> {
        let mut terminal = Terminal::default();
        terminal.interactive = false;
        let view = (80, "thinking".to_owned(), Some("context".to_owned()));
        terminal.activity_view.replace(Some(view.clone()));
        terminal.clear_activity()?;
        assert!(terminal.activity_view.borrow().is_none());
        terminal.activity_view.replace(Some(view.clone()));
        terminal.apply_ui(crate::ui::UiOptions::preset(1))?;
        assert!(terminal.activity_view.borrow().is_none());
        terminal.activity_view.replace(Some(view));
        let terminal = terminal.with_skin(crate::ui::UiOptions::preset(2));
        assert!(terminal.activity_view.borrow().is_none());
        Ok(())
    }

    #[test]
    fn message_wrapping_respects_label_space_wide_text_and_long_paths() {
        for text in [
            "Saved for future tasks in this workspace. No model call needed.",
            "日本語の長い説明です",
            "really/long/path/without/spaces/file.rs",
        ] {
            let lines = wrap_text(text, 12, 18);
            assert!(lines[0].width() <= 12);
            assert!(lines.iter().skip(1).all(|line| line.width() <= 18));
            assert_eq!(lines.join("").replace(' ', ""), text.replace(' ', ""));
        }
        assert_eq!(
            wrap_text("first\n\nsecond", 20, 20),
            vec!["first", "", "second"]
        );
        assert_eq!(wrap_text("日", 1, 1), vec!["?"]);
    }

    #[test]
    fn provider_errors_are_actionable_without_dumping_json_or_credentials() {
        let expired = r#"provider failed: {"error":{"type":"authentication_error","message":"OAuth access token has expired"},"token":"private-value"}"#;
        let message = friendly_error(expired);
        assert!(message.contains("F4"));
        assert!(!message.contains("private-value"));
        assert!(!message.contains('{'));
        assert!(friendly_error("HTTP 429: quota exhausted").contains("usage limit"));
        assert!(friendly_error("model_not_found").contains("F6"));
        assert!(!friendly_error(r#"failed: {"arbitrary":"secret"}"#).contains("secret"));
        assert_eq!(
            friendly_error("file not found: src/main.rs"),
            "file not found: src/main.rs"
        );
        assert!(friendly_error(&"failure ".repeat(100)).width() <= 240);
    }

    #[test]
    fn menu_filter_preserves_original_choice_indices() {
        let choices = ["Provider default", "Opus", "Sonnet", "Haiku"].map(str::to_owned);
        assert_eq!(menu_matches(&choices, "SON"), vec![2]);
        assert_eq!(menu_matches(&choices, ""), vec![0, 1, 2, 3]);
        assert!(menu_matches(&choices, "missing").is_empty());
    }

    #[test]
    fn result_feedback_reports_exit_status_and_virtualized_size_without_json() {
        let payload = serde_json::json!({"artifact":"0123456789abcdef", "detail":{"capability":"process.run","target":"node","exit_code":1,"output_bytes":20*1024*1024,"elapsed_ms":3800,"preview":"{private-json}"}});
        let (tone, label, text) = result_summary(&payload);
        assert!(matches!(tone, Tone::Warning));
        assert_eq!(label, "Command failed");
        assert!(text.contains("exit 1") && text.contains("20.0 MiB") && text.contains("3.8s"));
        assert!(!text.contains("private-json"));
        let (tone, label, _) = result_summary(
            &serde_json::json!({"detail":{"capability":"process.run","exit_code":0}}),
        );
        assert!(matches!(tone, Tone::Success));
        assert_eq!(label, "Command finished");
        assert!(matches!(
            result_summary(
                &serde_json::json!({"detail":{"capability":"process.run","exit_code":null}})
            )
            .0,
            Tone::Warning
        ));
    }

    #[test]
    fn footer_labels_character_counts_and_unknown_measurements_honestly() {
        let status = ContextStatus {
            prompt_chars: Some(18400),
            normalized_tokens: None,
            schema_count: Some(4),
            operations: 23,
            artifacts: 7,
            checkpoint_at: Some(100),
        };
        let text = status.text(112);
        assert!(text.contains("18400 chars") && text.contains("schemas 4"));
        assert!(
            text.contains("ops 23")
                && text.contains("artifacts 7")
                && text.contains("checkpoint 12s")
        );
        assert!(!text.contains("tokens"));
        assert!(ContextStatus::default().text(112).contains("ctx ? chars"));
        let measured = ContextStatus {
            normalized_tokens: Some(4200),
            ..status
        };
        assert!(measured.text(112).contains("ctx 4200 o200k units"));
        assert!(!measured.text(112).contains("4200 chars"));
    }

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
    fn menu_drafts_restore_once_with_caret_and_never_enter_secret_or_other_fields() {
        let terminal = Terminal::default();
        let text: Vec<_> = "fix 🦊\nthen test".chars().collect();
        terminal
            .input_draft
            .replace(Some(("task › ".into(), text.clone(), 5)));
        assert_eq!(terminal.take_input_draft("task › ", true), (vec![], 0));
        assert_eq!(terminal.take_input_draft("API key › ", false), (vec![], 0));
        assert_eq!(terminal.take_input_draft("task › ", false), (text, 5));
        assert_eq!(terminal.take_input_draft("task › ", false), (vec![], 0));
        terminal
            .input_draft
            .replace(Some(("task › ".into(), vec!['🦊'], 99)));
        assert_eq!(terminal.take_input_draft("task › ", false), (vec!['🦊'], 1));
    }

    #[test]
    fn custom_prompt_styles_preserve_task_drafts_without_relabeling_other_fields() -> Result<()> {
        let mut terminal = Terminal::default();
        let text: Vec<_> = "keep this 🦊 task".chars().collect();
        terminal
            .input_draft
            .replace(Some((terminal.input_prefix(), text.clone(), 5)));
        terminal.apply_ui(crate::ui::UiOptions {
            input_prefix: "  build › ".into(),
            ..Default::default()
        })?;
        terminal = terminal.with_skin(crate::ui::UiOptions {
            input_prefix: "  next › ".into(),
            ..Default::default()
        });
        assert_eq!(terminal.take_input_draft("  next › ", true), (vec![], 0));
        assert_eq!(terminal.take_input_draft("  next › ", false), (text, 5));
        terminal
            .input_draft
            .replace(Some(("  First line › ".into(), vec!['2'], 1)));
        terminal.apply_ui(crate::ui::UiOptions::default())?;
        assert_eq!(
            terminal.take_input_draft(&terminal.input_prefix(), false),
            (vec![], 0)
        );
        assert_eq!(
            terminal.take_input_draft("  First line › ", false),
            (vec!['2'], 1)
        );
        Ok(())
    }

    #[test]
    fn clips_unicode_to_terminal_width_and_removes_control_sequences() {
        assert_eq!(fit("こんにちは", 6), "こんに");
        assert_eq!(fit("hello", 3), "hel");
        assert!(!clean("text\u{1b}[2J\u{202e}spoof").contains('\u{1b}'));
        assert!(!clean("text\u{202e}spoof").contains('\u{202e}'));
    }
}
