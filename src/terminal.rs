use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::storage::Event as RunEvent;
pub use crate::text::clean;

const MAX_INPUT_BYTES: usize = 65_536;

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
    activity_view: std::cell::RefCell<Option<(usize, usize, String, Option<String>)>>,
    input_rows: std::cell::Cell<u16>,
    input_caret_row: std::cell::Cell<u16>,
    task_started_at: std::cell::Cell<Option<i64>>,
    input_draft: std::cell::RefCell<Option<(String, Vec<char>, usize)>>,
    input_status: std::cell::RefCell<String>,
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

struct InputHistory<'a> {
    entries: &'a [String],
    index: usize,
    draft: Option<(Vec<char>, usize)>,
}

#[derive(Debug)]
struct EditorView {
    rows: Vec<String>,
    caret_row: usize,
    caret_column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSize {
    columns: usize,
    rows: usize,
}

impl TerminalSize {
    fn measured() -> Option<Self> {
        terminal::size().ok().map(|(columns, rows)| Self {
            columns: columns as usize,
            rows: rows as usize,
        })
    }

    fn current() -> Self {
        Self::measured().unwrap_or(Self {
            columns: 80,
            rows: 24,
        })
    }

    fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::Resize(columns, rows) => Some(Self {
                columns: *columns as usize,
                rows: *rows as usize,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct InputLayout {
    label: String,
    composer_width: u16,
    max_input_rows: usize,
    available: usize,
}

fn activity_view_key(
    width: usize,
    height: usize,
    text: String,
    footer: Option<String>,
) -> (usize, usize, String, Option<String>) {
    (width, height, text, footer)
}

fn input_layout(size: TerminalSize, label: &str, composer: bool) -> InputLayout {
    let width = size.columns.max(1);
    let visible_label = fit(
        &if composer {
            format!("  │ {}", label.trim_start())
        } else {
            label.to_owned()
        },
        width.saturating_sub(4),
    );
    let composer_width = width.saturating_sub(4).max(4).min(u16::MAX as usize) as u16;
    let max_input_rows = size.rows.saturating_sub(10).clamp(1, 6);
    let available = if composer {
        crate::widgets::composer_input_area(
            label,
            ratatui::layout::Rect::new(0, 0, composer_width, (max_input_rows + 2) as u16),
        )
        .width as usize
    } else {
        width.saturating_sub(visible_label.width() + 1)
    }
    .max(1);
    InputLayout {
        label: visible_label,
        composer_width,
        max_input_rows,
        available,
    }
}

fn composer_draw_rows(previous: usize, current: usize) -> usize {
    previous.max(current)
}

fn logical_line_start(text: &[char], caret: usize) -> usize {
    let caret = caret.min(text.len());
    text[..caret]
        .iter()
        .rposition(|character| *character == '\n')
        .map_or(0, |index| index + 1)
}

fn logical_line_end(text: &[char], caret: usize) -> usize {
    let caret = caret.min(text.len());
    text[caret..]
        .iter()
        .position(|character| *character == '\n')
        .map_or(text.len(), |offset| caret + offset)
}

fn input_bytes(text: &[char]) -> usize {
    text.iter().fold(0usize, |bytes, character| {
        bytes.saturating_add(character.len_utf8())
    })
}

fn can_insert_input(current_bytes: usize, inserted_bytes: usize) -> bool {
    current_bytes.saturating_add(inserted_bytes) <= MAX_INPUT_BYTES
}

fn insert_input(
    text: &mut Vec<char>,
    caret: &mut usize,
    character: char,
    bytes: &mut usize,
) -> bool {
    let added_bytes = character.len_utf8();
    if !can_insert_input(*bytes, added_bytes) {
        return false;
    }
    text.insert(*caret, character);
    *caret += 1;
    *bytes += added_bytes;
    true
}

fn normalize_paste(paste: &str) -> String {
    const TAB_STOP: usize = 4;

    let normalized = paste.replace("\r\n", "\n").replace('\r', "\n");
    let mut expanded = String::with_capacity(normalized.len());
    let mut column = 0usize;
    for character in normalized.chars() {
        match character {
            '\n' => {
                expanded.push('\n');
                column = 0;
            }
            '\t' => {
                let spaces = TAB_STOP - column % TAB_STOP;
                for _ in 0..spaces {
                    expanded.push(' ');
                }
                column += spaces;
            }
            character => {
                expanded.push(character);
                column += character.width().unwrap_or(0);
            }
        }
    }
    clean(&expanded)
}

fn editor_view(text: &[char], caret: usize, width: usize, height: usize) -> EditorView {
    let width = width.max(1);
    let height = height.max(1);
    let mut rows = vec![String::new()];
    let mut positions = vec![(0usize, 0usize); text.len() + 1];
    let mut row = 0;
    let mut column = 0;

    for (index, character) in text.iter().copied().enumerate() {
        if character == '\n' {
            positions[index] = (row, column);
            row += 1;
            rows.push(String::new());
            column = 0;
            positions[index + 1] = (row, column);
            continue;
        }

        let mut rendered = character;
        let mut cells = character.width().unwrap_or(0);
        if cells > 0 && column + cells > width {
            row += 1;
            rows.push(String::new());
            column = 0;
            positions[index] = (row, column);
        }
        if cells > width {
            rendered = '?';
            cells = 1;
        }
        rows[row].push(rendered);
        column += cells;
        positions[index + 1] = (row, column);
    }

    let caret = caret.min(text.len());
    let (mut caret_row, mut caret_column) = positions[caret];
    if caret == text.len() && caret_column >= width {
        caret_row += 1;
        caret_column = 0;
        rows.push(String::new());
    }
    while rows.len() <= caret_row {
        rows.push(String::new());
    }
    let scroll = caret_row.saturating_add(1).saturating_sub(height);
    EditorView {
        rows: rows.into_iter().skip(scroll).take(height).collect(),
        caret_row: caret_row.saturating_sub(scroll),
        caret_column: caret_column.min(width.saturating_sub(1)),
    }
}

fn editor_vertical_move(
    text: &[char],
    caret: usize,
    width: usize,
    direction: KeyCode,
    preferred_column: Option<usize>,
) -> Option<(usize, usize)> {
    let width = width.max(1);
    let mut positions = vec![(0usize, 0usize); text.len() + 1];
    let mut row = 0;
    let mut column = 0;
    for (index, character) in text.iter().copied().enumerate() {
        if character == '\n' {
            positions[index] = (row, column);
            row += 1;
            column = 0;
            positions[index + 1] = (row, column);
            continue;
        }
        let cells = character.width().unwrap_or(0).min(width);
        if cells > 0 && column + cells > width {
            row += 1;
            column = 0;
            positions[index] = (row, column);
        }
        column += cells;
        positions[index + 1] = (row, column);
    }

    let caret = caret.min(text.len());
    let (mut current_row, mut current_column) = positions[caret];
    if caret == text.len() && current_column >= width {
        current_row += 1;
        current_column = 0;
    }
    let target_row = match direction {
        KeyCode::Up if current_row > 0 => current_row - 1,
        KeyCode::Down if current_row < row => current_row + 1,
        _ => return None,
    };
    let desired = preferred_column.unwrap_or(current_column);
    positions
        .iter()
        .enumerate()
        .filter_map(|(index, (candidate_row, candidate_column))| {
            let (candidate_row, candidate_column) =
                if index == text.len() && *candidate_column >= width {
                    (*candidate_row + 1, 0)
                } else {
                    (*candidate_row, *candidate_column)
                };
            (candidate_row == target_row).then_some((index, candidate_column))
        })
        .min_by_key(|(index, candidate_column)| {
            (
                candidate_column.abs_diff(desired),
                std::cmp::Reverse(*index),
            )
        })
        .map(|(index, candidate_column)| (index, candidate_column))
}

impl<'a> InputHistory<'a> {
    fn new(entries: &'a [String]) -> Self {
        Self {
            entries,
            index: entries.len(),
            draft: None,
        }
    }

    fn navigate(&mut self, key: KeyCode, text: &mut Vec<char>, caret: &mut usize) {
        match key {
            KeyCode::Up if self.index > 0 => {
                if self.index == self.entries.len() {
                    self.draft = Some((text.clone(), (*caret).min(text.len())));
                }
                self.index -= 1;
            }
            KeyCode::Down if self.index < self.entries.len() => self.index += 1,
            _ => return,
        }
        if let Some(entry) = self.entries.get(self.index) {
            *text = entry.chars().collect();
            *caret = text.len();
        } else if let Some((draft, position)) = self.draft.take() {
            *text = draft;
            *caret = position;
        }
    }

    fn reset(&mut self) {
        self.index = self.entries.len();
        self.draft = None;
    }
}

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
    if [
        "timed out",
        "timeout",
        "deadline elapsed",
        "deadline exceeded",
        "deadline reached",
    ]
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
            input_rows: std::cell::Cell::new(3),
            input_caret_row: std::cell::Cell::new(1),
            task_started_at: std::cell::Cell::new(None),
            input_draft: std::cell::RefCell::new(None),
            input_status: std::cell::RefCell::new(String::new()),
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
        if self.colors {
            queue!(output, SetForegroundColor(self.color(tone)))?;
        }
        write!(output, "  {text}\r\n")?;
        if self.colors {
            queue!(output, ResetColor)?;
        }
        output.flush()?;
        Ok(())
    }

    pub fn welcome(&self, provider: &str, workspace: &str) -> Result<()> {
        self.home(provider, workspace, "", &[])
    }

    pub fn home(
        &self,
        provider: &str,
        workspace: &str,
        selection: &str,
        recent: &[String],
    ) -> Result<()> {
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80)
            .saturating_sub(4)
            .max(12);
        let mut output = io::stdout();
        write!(output, "\r\n")?;
        let buffer = self.home_buffer(provider, workspace, selection, recent, width);
        for row in 0..buffer.area.height {
            if self.interactive {
                self.paint_widget_row(&buffer, row, 2)?;
            } else {
                write!(output, "  {}", crate::widgets::row_text(&buffer, row))?;
            }
            write!(output, "\r\n")?;
        }
        if self.colors {
            queue!(output, ResetColor)?;
        }
        write!(output, "\r\n")?;
        if self
            .skin
            .blocks()
            .contains(&crate::ui::WelcomeBlock::Shortcuts)
        {
            self.message(
                Tone::Quiet,
                "",
                "F2 provider  F4 sign in  F6 model/reasoning  F3 chats  F7 style  F1 help",
            )?;
            write!(output, "\r\n")?;
        }
        output.flush()?;
        Ok(())
    }

    fn home_buffer(
        &self,
        provider: &str,
        workspace: &str,
        selection: &str,
        recent: &[String],
        width: usize,
    ) -> ratatui::buffer::Buffer {
        let width = width.max(12);
        let blocks = self.skin.blocks();
        let mut content = Vec::new();
        for block in &blocks {
            match block {
                crate::ui::WelcomeBlock::Mascot => {
                    let portrait = self.skin.portrait();
                    if portrait.is_empty() {
                        content.push("Aegis · ready when you are".into());
                    }
                    for (index, row) in portrait.into_iter().enumerate() {
                        content.push(if row.width() > 10 {
                            row
                        } else {
                            format!(
                                "{}  {}",
                                pad_line(&row, 10),
                                ["Ready when you are.", "Let's build it.", "", ""]
                                    .get(index)
                                    .copied()
                                    .unwrap_or("")
                            )
                        });
                    }
                    content.push(String::new());
                }
                crate::ui::WelcomeBlock::Provider => {
                    content.push(provider.to_owned());
                    if !selection.is_empty() {
                        content.push(selection.to_owned());
                    }
                }
                crate::ui::WelcomeBlock::Hint => content.push("Ask, explore, build.".into()),
                _ => {}
            }
        }
        let mut history = vec!["Recent chats  /  F3".into(), String::new()];
        if recent.is_empty() {
            history.extend([
                "A fresh canvas.".into(),
                "Your chats save automatically.".into(),
            ]);
        } else {
            history.extend(recent.iter().take(6).cloned());
        }
        crate::widgets::home(
            &content,
            &history,
            blocks
                .contains(&crate::ui::WelcomeBlock::Workspace)
                .then_some(workspace),
            width.min(u16::MAX as usize) as u16,
            &self.skin.palette(),
        )
    }

    fn paint_widget_row(
        &self,
        buffer: &ratatui::buffer::Buffer,
        row: u16,
        margin: u16,
    ) -> Result<()> {
        use ratatui::style::{Color as WidgetColor, Modifier};
        let mut output = io::stdout();
        queue!(
            output,
            cursor::MoveToColumn(0),
            Clear(ClearType::CurrentLine)
        )?;
        write!(output, "{}", " ".repeat(margin as usize))?;
        let mut previous = None;
        let mut column = 0;
        while column < buffer.area.width {
            let cell = &buffer[(column, row)];
            let style = (cell.fg, cell.bg, cell.modifier);
            if self.colors && previous != Some(style) {
                let convert = |color| match color {
                    WidgetColor::Rgb(r, g, b) => Color::Rgb { r, g, b },
                    _ => Color::Reset,
                };
                queue!(
                    output,
                    SetAttribute(Attribute::Reset),
                    SetForegroundColor(convert(cell.fg)),
                    SetBackgroundColor(convert(cell.bg))
                )?;
                for (modifier, attribute) in [
                    (Modifier::BOLD, Attribute::Bold),
                    (Modifier::DIM, Attribute::Dim),
                    (Modifier::REVERSED, Attribute::Reverse),
                ] {
                    if cell.modifier.contains(modifier) {
                        queue!(output, SetAttribute(attribute))?;
                    }
                }
                previous = Some(style);
            }
            write!(output, "{}", cell.symbol())?;
            column += cell.symbol().width().max(1) as u16;
        }
        if self.colors {
            queue!(output, SetAttribute(Attribute::Reset), ResetColor)?;
        }
        Ok(())
    }

    pub fn set_input_status(&self, status: &str) {
        self.input_status.replace(clean(status));
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

    pub fn set_task_started_at(&self, timestamp: Option<i64>) {
        self.task_started_at.set(timestamp);
    }

    fn completion_timing(&self, completed_at: i64) -> String {
        let completed = format_utc(completed_at);
        match self.task_started_at.get() {
            Some(started_at) => format!(
                "Completed {completed} · Worked for {}",
                format_duration(Duration::from_secs(
                    completed_at.saturating_sub(started_at).max(0) as u64
                ))
            ),
            None => format!("Completed {completed}"),
        }
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
        let now = crate::storage::unix_time();
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80);
        let text = format!(
            "  {frame} {} · {} elapsed · {}",
            fit(label, 16),
            format_duration(elapsed),
            format_clock_utc(now)
        );
        let text = fit(&text, width.saturating_sub(1));
        let mut footer = "  Type/paste to steer · ^C interrupt · ^D detach".to_owned();
        if self.skin.show_context() {
            footer.push_str(&format!(
                " · {} · {} recorded tokens",
                context.text(now),
                tokens
            ));
        }
        let footer = fit(&footer, width.saturating_sub(1));
        self.paint_activity(width, text, Some(footer))
    }

    pub fn authentication_activity(&self, label: &str, elapsed: Duration) -> Result<()> {
        self.loading_activity(
            label,
            elapsed,
            "Esc / Ctrl+C cancel · sign-in codes never enter chat",
        )
    }

    pub fn loading_activity(&self, label: &str, elapsed: Duration, hint: &str) -> Result<()> {
        if !self.interactive {
            return Ok(());
        }
        let width = terminal::size()
            .map(|(width, _)| width as usize)
            .unwrap_or(80);
        let tick = if self.animations {
            (elapsed.as_millis() / self.skin.frame_interval().as_millis().max(1)) as u64
        } else {
            0
        };
        let frame = fit(&self.skin.frame(crate::ui::Phase::Working, tick), 24);
        self.paint_activity(
            width,
            fit(&format!("  {frame} {label}"), width.saturating_sub(1)),
            Some(fit(&format!("  {hint}"), width.saturating_sub(1))),
        )
    }

    fn paint_activity(&self, width: usize, text: String, footer: Option<String>) -> Result<()> {
        let view = activity_view_key(width, TerminalSize::current().rows, text, footer);
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
        write!(io::stdout(), "{}", view.2)?;
        queue!(io::stdout(), ResetColor)?;
        if view.3.is_none() {
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
        write!(io::stdout(), "{}", view.3.as_deref().unwrap_or_default())?;
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

    pub fn command_menu(&self) -> Result<Option<String>> {
        let choices = crate::commands::COMMANDS
            .iter()
            .map(|(command, description)| format!("{command:<24} {description}"))
            .collect::<Vec<_>>();
        let title = format!(
            "Slash commands ({}) · search or scroll · Enter fills the prompt",
            choices.len()
        );
        Ok(self
            .select(&title, &choices)?
            .map(|index| crate::commands::COMMANDS[index].0.to_owned()))
    }

    pub fn set_command_draft(&self, command: &str) {
        self.set_input_draft(&selected_command_draft(command));
    }

    pub fn set_input_draft(&self, draft: &str) {
        let text: Vec<_> = draft.chars().collect();
        let caret = text.len();
        self.input_draft
            .replace(Some((self.input_prefix(), text, caret)));
    }

    pub fn input(&self, label: &str, secret: bool, history: &[String]) -> Result<Input> {
        if !self.interactive {
            print!("{label}");
            io::stdout().flush()?;
            let mut text = String::new();
            if io::stdin().read_line(&mut text)? == 0 {
                return Ok(Input::Exit);
            }
            if !secret && text.starts_with("\u{1b}[200~") {
                text.drain(..6);
                while !text.contains("\u{1b}[201~") {
                    if text.len() > MAX_INPUT_BYTES {
                        anyhow::bail!("Pasted input exceeds {MAX_INPUT_BYTES} bytes");
                    }
                    let mut line = String::new();
                    if io::stdin().read_line(&mut line)? == 0 {
                        anyhow::bail!("Pasted input ended before its closing marker");
                    }
                    text.push_str(&line);
                }
                let (paste, tail) = text.split_once("\u{1b}[201~").unwrap();
                let normalized = normalize_paste(paste);
                if !tail.trim().is_empty() || !can_insert_input(0, normalized.len()) {
                    anyhow::bail!("Pasted input has trailing data or exceeds its limit");
                }
                return Ok(Input::Submit(normalized));
            }
            let input = text.trim_end_matches(['\r', '\n']);
            if !can_insert_input(0, input.len()) {
                anyhow::bail!("Input exceeds {MAX_INPUT_BYTES} bytes");
            }
            return Ok(Input::Submit(input.into()));
        }
        let _raw = RawMode::enter(true)?;
        let composer =
            !secret && label == self.input_prefix() && !self.input_status.borrow().is_empty();
        let draft_label = label.to_owned();
        let (mut text, mut caret) = self.take_input_draft(label, secret);
        let mut draft_bytes = input_bytes(&text);
        let mut input_history = InputHistory::new(history);
        let mut rendered_composer = false;
        let mut previous_draw_rows = 0usize;
        let mut previous_caret_row = 1usize;
        let mut preferred_column = None;
        let mut paste_notice = false;
        let mut screen_size = TerminalSize::current();
        let mut pending_resize = None;
        let mut redraw_from_current_line = false;
        loop {
            if let Some(resized) = pending_resize.take() {
                screen_size = resized;
            } else if let Some(measured) = TerminalSize::measured() {
                screen_size = measured;
            }
            let layout = input_layout(screen_size, &draft_label, composer);
            let label = &layout.label;
            let composer_width = layout.composer_width;
            let max_input_rows = layout.max_input_rows;
            let available = layout.available;
            let displayed: Vec<_> = text
                .iter()
                .map(|character| {
                    if secret {
                        '●'
                    } else if *character == '\n' && !composer {
                        '↵'
                    } else if character.is_control() {
                        ' '
                    } else {
                        *character
                    }
                })
                .collect();
            let (input_column, caret_width) = if composer {
                let editor = editor_view(&displayed, caret, available, max_input_rows);
                let status = if paste_notice {
                    format!("{} · input exceeds 64 KiB", self.input_status.borrow())
                } else {
                    self.input_status.borrow().clone()
                };
                let (buffer, origin) = crate::widgets::composer(
                    &draft_label,
                    &editor.rows,
                    &status,
                    composer_width,
                    &self.skin.palette(),
                );
                let margin = 2;
                let draw_rows = composer_draw_rows(previous_draw_rows, buffer.area.height as usize);
                if rendered_composer {
                    queue!(io::stdout(), cursor::MoveUp(previous_caret_row as u16))?;
                } else if redraw_from_current_line {
                    queue!(io::stdout(), cursor::MoveToColumn(0))?;
                } else {
                    write!(io::stdout(), "\r\n")?;
                }
                for row in 0..draw_rows {
                    if row < buffer.area.height as usize {
                        self.paint_widget_row(&buffer, row as u16, margin)?;
                    } else {
                        queue!(
                            io::stdout(),
                            cursor::MoveToColumn(0),
                            Clear(ClearType::CurrentLine)
                        )?;
                    }
                    if row + 1 < draw_rows {
                        write!(io::stdout(), "\r\n")?;
                    }
                }
                let caret_row = 1 + editor.caret_row;
                queue!(
                    io::stdout(),
                    cursor::MoveUp(draw_rows.saturating_sub(1 + caret_row) as u16),
                    ResetColor
                )?;
                self.input_rows.set(draw_rows.min(u16::MAX as usize) as u16);
                self.input_caret_row.set(caret_row as u16);
                previous_draw_rows = draw_rows;
                previous_caret_row = caret_row;
                rendered_composer = true;
                redraw_from_current_line = false;
                (composer_input_column(origin, margin), editor.caret_column)
            } else {
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
                (label.width(), caret_width)
            };
            queue!(
                io::stdout(),
                cursor::MoveToColumn((input_column + caret_width) as u16)
            )?;
            io::stdout().flush()?;
            match event::read()? {
                Event::Paste(paste) => {
                    let normalized = normalize_paste(&paste);
                    if !can_insert_input(draft_bytes, normalized.len()) {
                        paste_notice = true;
                    } else {
                        let characters: Vec<_> = normalized.chars().collect();
                        text.splice(caret..caret, characters.iter().copied());
                        caret += characters.len();
                        draft_bytes += normalized.len();
                        paste_notice = false;
                    }
                }
                Event::Resize(width, height) => {
                    let event = Event::Resize(width, height);
                    screen_size = TerminalSize::from_event(&event).unwrap_or(screen_size);
                    pending_resize = Some(screen_size);
                    if composer && rendered_composer {
                        queue!(
                            io::stdout(),
                            cursor::MoveUp(previous_caret_row as u16),
                            cursor::MoveToColumn(0),
                            Clear(ClearType::FromCursorDown)
                        )?;
                        io::stdout().flush()?;
                        rendered_composer = false;
                        previous_draw_rows = 0;
                        redraw_from_current_line = true;
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !secret && matches!(key.code, KeyCode::F(1..=9)) {
                        self.input_draft
                            .replace(Some((draft_label.clone(), text.clone(), caret)));
                    }
                    match key.code {
                        KeyCode::Enter
                            if composer && key.modifiers.contains(KeyModifiers::SHIFT) =>
                        {
                            paste_notice =
                                !insert_input(&mut text, &mut caret, '\n', &mut draft_bytes);
                            preferred_column = None;
                        }
                        KeyCode::Char('j') if composer && control => {
                            paste_notice =
                                !insert_input(&mut text, &mut caret, '\n', &mut draft_bytes);
                            preferred_column = None;
                        }
                        KeyCode::Enter => {
                            if !can_insert_input(draft_bytes, 0) {
                                paste_notice = true;
                                continue;
                            }
                            let submitted: String = text.iter().collect();
                            self.finish_input(
                                composer,
                                (!secret && !submitted.starts_with('/'))
                                    .then_some(submitted.as_str()),
                            )?;
                            return Ok(Input::Submit(submitted));
                        }
                        KeyCode::Char('d') if control && text.is_empty() => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Exit);
                        }
                        KeyCode::Char('c') if control => {
                            text.clear();
                            caret = 0;
                            draft_bytes = 0;
                            paste_notice = false;
                            input_history.reset();
                            preferred_column = None;
                        }
                        KeyCode::Char('u') if control => {
                            text.clear();
                            caret = 0;
                            draft_bytes = 0;
                            paste_notice = false;
                            input_history.reset();
                            preferred_column = None;
                        }
                        KeyCode::F(2) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Providers);
                        }
                        KeyCode::F(3) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Sessions);
                        }
                        KeyCode::F(4) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Login);
                        }
                        KeyCode::F(5) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::NewConversation);
                        }
                        KeyCode::F(6) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Models);
                        }
                        KeyCode::F(7) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Settings);
                        }
                        KeyCode::F(1) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Help);
                        }
                        KeyCode::F(8) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::Checkpoint);
                        }
                        KeyCode::F(9) if !secret => {
                            self.finish_input(composer, None)?;
                            return Ok(Input::CancelTask);
                        }
                        KeyCode::Left => {
                            caret = caret.saturating_sub(1);
                            preferred_column = None;
                        }
                        KeyCode::Right => {
                            caret = (caret + 1).min(text.len());
                            preferred_column = None;
                        }
                        KeyCode::Home => {
                            caret = logical_line_start(&text, caret);
                            preferred_column = None;
                        }
                        KeyCode::End => {
                            caret = logical_line_end(&text, caret);
                            preferred_column = None;
                        }
                        KeyCode::Backspace if caret > 0 => {
                            caret -= 1;
                            let removed = text.remove(caret);
                            draft_bytes -= removed.len_utf8();
                            paste_notice = draft_bytes > MAX_INPUT_BYTES;
                            preferred_column = None;
                        }
                        KeyCode::Delete if caret < text.len() => {
                            let removed = text.remove(caret);
                            draft_bytes -= removed.len_utf8();
                            paste_notice = draft_bytes > MAX_INPUT_BYTES;
                            preferred_column = None;
                        }
                        KeyCode::Up | KeyCode::Down if !secret => {
                            if composer {
                                let target = editor_vertical_move(
                                    &displayed,
                                    caret,
                                    available,
                                    key.code,
                                    preferred_column,
                                );
                                if let Some((next, column)) = target {
                                    caret = next;
                                    preferred_column = Some(column);
                                } else {
                                    input_history.navigate(key.code, &mut text, &mut caret);
                                    draft_bytes = input_bytes(&text);
                                    paste_notice = draft_bytes > MAX_INPUT_BYTES;
                                    preferred_column = None;
                                }
                            } else {
                                input_history.navigate(key.code, &mut text, &mut caret);
                                draft_bytes = input_bytes(&text);
                                paste_notice = draft_bytes > MAX_INPUT_BYTES;
                            }
                        }
                        KeyCode::Char('/')
                            if !control
                                && !secret
                                && text.is_empty()
                                && draft_label == self.input_prefix() =>
                        {
                            self.finish_input(composer, None)?;
                            let selected = self.command_menu()?.unwrap_or_else(|| "/".into());
                            text = selected_command_draft(&selected).chars().collect();
                            caret = text.len();
                            draft_bytes = input_bytes(&text);
                            paste_notice = draft_bytes > MAX_INPUT_BYTES;
                            rendered_composer = false;
                            previous_draw_rows = 0;
                            preferred_column = None;
                        }
                        KeyCode::Char(character) if !control => {
                            paste_notice =
                                !insert_input(&mut text, &mut caret, character, &mut draft_bytes);
                            preferred_column = None;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn finish_input(&self, composer: bool, submitted: Option<&str>) -> Result<()> {
        if !composer {
            write!(io::stdout(), "\r\n")?;
            return Ok(());
        }
        let rows = self.input_rows.replace(3).max(1);
        let caret_row = self.input_caret_row.replace(1).min(rows.saturating_sub(1));
        queue!(
            io::stdout(),
            cursor::MoveUp(caret_row),
            cursor::MoveToColumn(0),
            ResetColor
        )?;
        for row in 0..rows {
            queue!(io::stdout(), Clear(ClearType::CurrentLine))?;
            if row + 1 < rows {
                queue!(io::stdout(), cursor::MoveDown(1), cursor::MoveToColumn(0))?;
            }
        }
        queue!(io::stdout(), cursor::MoveUp(rows - 1), cursor::Show)?;
        if let Some(text) = submitted.filter(|text| !text.is_empty()) {
            self.message(Tone::Accent, "You", text)?;
            write!(io::stdout(), "\r\n")?;
        }
        io::stdout().flush()?;
        Ok(())
    }

    pub fn select(&self, title: &str, choices: &[String]) -> Result<Option<usize>> {
        self.select_at(title, choices, 0)
    }

    pub fn select_at(
        &self,
        title: &str,
        choices: &[String],
        initial: usize,
    ) -> Result<Option<usize>> {
        if choices.is_empty() {
            return Ok(None);
        }
        if self.interactive {
            return self.menu(title, choices, initial);
        }
        self.message(Tone::Accent, "◇", title)?;
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

    fn menu(&self, title: &str, choices: &[String], initial: usize) -> Result<Option<usize>> {
        let _raw = RawMode::enter(true)?;
        let mut selected = initial.min(choices.len() - 1);
        let mut digits = String::new();
        let mut query = String::new();
        let mut rendered = false;
        let mut rendered_rows = 0usize;
        let mut screen_size = TerminalSize::current();
        let mut pending_resize = None;
        let mut redraw_from_current_line = false;
        loop {
            if let Some(resized) = pending_resize.take() {
                screen_size = resized;
            } else if let Some(measured) = TerminalSize::measured() {
                screen_size = measured;
            }
            let width = screen_size.columns.min(u16::MAX as usize) as u16;
            let rows = menu_rows(screen_size.rows, choices.len());
            if rendered {
                queue!(
                    io::stdout(),
                    cursor::MoveUp(rendered_rows.saturating_sub(1) as u16)
                )?;
            } else if redraw_from_current_line {
                queue!(io::stdout(), cursor::MoveToColumn(0))?;
            }
            let matches = menu_matches(choices, &query);
            selected = selected.min(matches.len().saturating_sub(1));
            let buffer = crate::widgets::menu(
                title,
                choices,
                &matches,
                selected,
                rows as u16,
                &query,
                width.saturating_sub(1).max(1),
                &self.skin.palette(),
            );
            for row in 0..buffer.area.height {
                self.paint_widget_row(&buffer, row, 0)?;
                if row + 1 < buffer.area.height {
                    write!(io::stdout(), "\r\n")?;
                }
            }
            queue!(
                io::stdout(),
                cursor::MoveToColumn(0),
                ResetColor,
                cursor::Hide
            )?;
            io::stdout().flush()?;
            rendered = true;
            rendered_rows = buffer.area.height as usize;
            redraw_from_current_line = false;
            match event::read()? {
                Event::Resize(width, height) => {
                    let event = Event::Resize(width, height);
                    screen_size = TerminalSize::from_event(&event).unwrap_or(screen_size);
                    pending_resize = Some(screen_size);
                    queue!(
                        io::stdout(),
                        cursor::MoveUp(rendered_rows.saturating_sub(1) as u16),
                        cursor::MoveToColumn(0),
                        Clear(ClearType::FromCursorDown)
                    )?;
                    io::stdout().flush()?;
                    rendered = false;
                    rendered_rows = 0;
                    redraw_from_current_line = true;
                }
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match key.code {
                        KeyCode::Enter => {
                            if let Some(index) = matches.get(selected) {
                                self.clear_menu(rows)?;
                                return Ok(Some(*index));
                            }
                        }
                        KeyCode::Esc => {
                            self.clear_menu(rows)?;
                            return Ok(None);
                        }
                        KeyCode::Char('c' | 'd')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.clear_menu(rows)?;
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
                        KeyCode::Char(character)
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
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
                _ => {}
            }
        }
    }

    fn clear_menu(&self, rows: usize) -> Result<()> {
        queue!(
            io::stdout(),
            cursor::MoveUp(rows as u16 + 1),
            cursor::MoveToColumn(0),
            ResetColor
        )?;
        for row in 0..rows + 2 {
            queue!(io::stdout(), Clear(ClearType::CurrentLine))?;
            if row < rows + 1 {
                queue!(io::stdout(), cursor::MoveDown(1), cursor::MoveToColumn(0))?;
            }
        }
        queue!(io::stdout(), cursor::MoveUp(rows as u16 + 1), cursor::Show)?;
        io::stdout().flush()?;
        Ok(())
    }

    pub fn render_event(&self, event: &RunEvent) -> Result<()> {
        let payload = &event.payload;
        match event.kind.as_str() {
            "run.running" => {
                if self.task_started_at.get().is_none() {
                    self.task_started_at.set(Some(event.created_at));
                }
                Ok(())
            }
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
                let detail = if capability == "workspace.read_batch" {
                    format!(
                        "{} requested files",
                        args["files"].as_array().map_or(0, Vec::len)
                    )
                } else {
                    detail.to_owned()
                };
                let label = match capability {
                    "workspace.read" => "Read",
                    "workspace.read_batch" => "Read batch",
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
                        fit(&detail, 160)
                    ),
                )
            }
            "operation.succeeded" => {
                let (tone, label, text) = result_summary(payload);
                self.message(tone, label, &text)?;
                if let Some((label, preview)) = operation_output_preview(payload) {
                    self.message(Tone::Quiet, &label, &preview)?;
                }
                Ok(())
            }
            "run.answered" => {
                self.message(
                    Tone::Accent,
                    "Aegis",
                    payload["summary"].as_str().unwrap_or_default(),
                )?;
                self.message(
                    Tone::Quiet,
                    "Goal time",
                    &self.completion_timing(event.created_at),
                )
            }
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
                )?;
                if payload["completion"].is_object() {
                    self.message(
                        Tone::Quiet,
                        "Completion evidence",
                        &crate::control::display_completion(&payload["completion"]),
                    )?;
                }
                self.message(
                    Tone::Quiet,
                    "Goal time",
                    &self.completion_timing(event.created_at),
                )
            }
            "conversation.inspected" => self.message(
                Tone::Quiet,
                "Recall",
                &format!(
                    "Saved chat · {} characters · context only",
                    event.payload["excerpt"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .count()
                ),
            ),
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
            "model.format_retry" => self.message(
                Tone::Quiet,
                "Retry",
                "Invalid model reply; asking once more without applying an action.",
            ),
            "provider.transition" => self.message(
                Tone::Accent,
                "Switching models",
                &format!(
                    "{} / {} → {} / {} · same task and evidence retained",
                    payload["from"]["provider"].as_str().unwrap_or("provider"),
                    payload["from"]["model"].as_str().unwrap_or("model"),
                    payload["to"]["provider"].as_str().unwrap_or("provider"),
                    payload["to"]["model"].as_str().unwrap_or("model")
                ),
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
            "pause.requested" => self.message(
                Tone::Quiet,
                "Pause requested",
                "The current action will record its outcome before inference stops.",
            ),
            "run.paused" | "run.waiting_recovery" | "run.failed" => self.message(
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

fn pad_line(text: &str, width: usize) -> String {
    let text = fit(&clean(text).replace('\n', " "), width);
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
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

pub(crate) fn selected_command_draft(command: &str) -> String {
    if matches!(command, "/goal" | "/contract") {
        format!("{command} ")
    } else {
        command.to_owned()
    }
}

fn menu_rows(terminal_height: usize, choices: usize) -> usize {
    terminal_height.saturating_sub(5).max(1).min(choices.max(1))
}

fn result_summary(payload: &serde_json::Value) -> (Tone, &'static str, String) {
    let detail = &payload["detail"];
    let capability = detail["capability"].as_str().unwrap_or("tool");
    let target = detail["target"].as_str().unwrap_or(capability);
    let mut text = fit(target, 100);
    if let Some(characters) = detail["selected_characters"].as_u64() {
        text.push_str(&format!(" · {characters} selected characters"));
    }
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
        "workspace.read_batch" => "Read batch",
        "workspace.search" => "Found",
        "process.run" => "Command finished",
        _ => "Tool finished",
    };
    (Tone::Success, label, text)
}

pub(crate) fn operation_output_preview(payload: &serde_json::Value) -> Option<(String, String)> {
    const MAX_DISPLAY_CHARACTERS: usize = 2400;
    let preview = &payload["detail"]["output_preview"];
    let preview_text = |value: &serde_json::Value| {
        let mut text = clean(value["text"].as_str().unwrap_or_default());
        if value["truncated"] == true {
            text.push_str("\n…");
        }
        text
    };
    let (label, text, truncated) = match preview["kind"].as_str()? {
        "text" => (
            if preview["source"] == "command" {
                "Command output"
            } else {
                "File excerpt"
            },
            preview_text(&preview["preview"]),
            preview["preview"]["truncated"] == true,
        ),
        "read_batch" => {
            let files = preview["files"].as_array()?;
            let text = files
                .iter()
                .map(|file| {
                    format!(
                        "{} · offset {}\n{}",
                        clean(file["path"].as_str().unwrap_or("file")),
                        file["offset"].as_u64().unwrap_or(0),
                        preview_text(&file["preview"])
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            ("Read excerpts", text, preview["truncated"] == true)
        }
        "search" => {
            let matches = preview["matches"].as_array()?;
            let text = matches
                .iter()
                .map(|item| {
                    format!(
                        "{}:{}  {}",
                        clean(item["path"].as_str().unwrap_or("file")),
                        item["line"].as_u64().unwrap_or(0),
                        preview_text(&item["preview"])
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("Search matches", text, preview["truncated"] == true)
        }
        "edit_summary" => {
            let mut details = vec![format!(
                "{} {}",
                preview["action"].as_str().unwrap_or("updated"),
                clean(preview["path"].as_str().unwrap_or("file"))
            )];
            if let Some(bytes) = preview["bytes"].as_u64() {
                details.push(format!("{bytes} bytes"));
            }
            if let Some(edits) = preview["edits"].as_u64() {
                details.push(format!("{edits} edits"));
            }
            if let Some(hash) = preview["sha256"].as_str() {
                details.push(format!("SHA256 {}", fit(hash, 12)));
            }
            ("Edit summary", details.join(" · "), false)
        }
        _ => return None,
    };
    if text.trim().is_empty() {
        return None;
    }
    let cleaned = clean(&text);
    let mut characters = cleaned.chars();
    let mut bounded = characters
        .by_ref()
        .take(MAX_DISPLAY_CHARACTERS)
        .collect::<String>();
    if truncated || characters.next().is_some() {
        bounded.push_str("\n… output preview truncated");
    }
    Some((label.to_owned(), bounded))
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

fn composer_input_column(origin: u16, margin: u16) -> usize {
    origin as usize + margin as usize
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

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn format_utc(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let shifted_days = days + 719_468;
    let era = (if shifted_days >= 0 {
        shifted_days
    } else {
        shifted_days - 146_096
    }) / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds / 3600;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn format_clock_utc(timestamp: i64) -> String {
    let seconds = timestamp.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_panels_align_at_narrow_and_wide_unicode_widths() {
        let terminal = Terminal::default();
        for width in [16, 32, 60, 72, 76, 96, 120, 160] {
            let buffer = terminal.home_buffer(
                "ChatGPT / Codex",
                "C:\\long\\workspace\\日本語",
                "example-model · high",
                &["• Fix parser 🦊".into(), "  paused · 4m ago".into()],
                width,
            );
            let rows: Vec<_> = (0..buffer.area.height)
                .map(|row| crate::widgets::row_text(&buffer, row))
                .collect();
            assert!(rows.iter().all(|row| row.width() == width), "{rows:?}");
            assert!(rows[0].starts_with('╭'));
            assert!(rows.last().unwrap().ends_with('╯'));
        }
        let style = crate::ui::UiOptions {
            blocks: vec![crate::ui::WelcomeBlock::Hint],
            ..Default::default()
        };
        let buffer = Terminal::default().with_skin(style).home_buffer(
            "hidden-provider",
            "hidden-path",
            "hidden-model",
            &[],
            76,
        );
        let rows = (0..buffer.area.height)
            .map(|row| crate::widgets::row_text(&buffer, row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rows.contains("hidden-"));
        assert!(rows.contains("Ask, explore, build."));
    }

    #[test]
    fn activity_cache_is_invalidated_after_clear_and_style_changes() -> Result<()> {
        let mut terminal = Terminal::default();
        terminal.interactive = false;
        let view = (80, 24, "thinking".to_owned(), Some("context".to_owned()));
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
        assert_eq!(
            friendly_error("Task and command deadlines are enforced."),
            "Task and command deadlines are enforced."
        );
        assert!(friendly_error("Model request deadline elapsed").contains("time limit"));
        assert!(friendly_error(&"failure ".repeat(100)).width() <= 240);
    }

    #[test]
    fn menu_filter_preserves_original_choice_indices() {
        let choices = ["Provider default", "Opus", "Sonnet", "Haiku"].map(str::to_owned);
        assert_eq!(menu_matches(&choices, "SON"), vec![2]);
        assert_eq!(menu_matches(&choices, ""), vec![0, 1, 2, 3]);
        assert!(menu_matches(&choices, "missing").is_empty());
        assert_eq!(menu_rows(24, 32), 19);
        assert_eq!(menu_rows(60, 32), 32);
        assert_eq!(menu_rows(4, 32), 1);
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
    fn structured_operation_previews_render_clean_bounded_content_and_edit_summaries() {
        let output = serde_json::json!({
            "detail": {"output_preview": {
                "kind":"text",
                "source":"command",
                "preview":{"text":format!("build passed\n{}\u{1b}[2J\u{202e}tail", "x".repeat(400)),"truncated":true}
            }}
        });
        let (label, preview) = operation_output_preview(&output).unwrap();
        assert_eq!(label, "Command output");
        assert!(preview.starts_with("build passed\n"));
        assert!(!preview.contains('\u{1b}') && !preview.contains('\u{202e}'));
        assert!(preview.chars().count() <= 2400 + "\n… output preview truncated".chars().count());
        assert!(preview.ends_with("output preview truncated"));

        let batch = serde_json::json!({"detail":{"output_preview":{"kind":"read_batch","files":[
            {"path":"src/a.rs","offset":10,"preview":{"text":"first file","truncated":false}},
            {"path":"src/b.rs","offset":20,"preview":{"text":"second file","truncated":false}}
        ],"truncated":false}}});
        let (label, preview) = operation_output_preview(&batch).unwrap();
        assert_eq!(label, "Read excerpts");
        assert!(preview.contains("src/a.rs · offset 10") && preview.contains("second file"));

        let matches = serde_json::json!({"detail":{"output_preview":{"kind":"search","matches":[
            {"path":"src/main.rs","line":7,"preview":{"text":"matched line","truncated":false}}
        ],"truncated":false}}});
        let (label, preview) = operation_output_preview(&matches).unwrap();
        assert_eq!(label, "Search matches");
        assert!(preview.contains("src/main.rs:7  matched line"));

        let edit = serde_json::json!({"detail":{"output_preview":{"kind":"edit_summary","action":"patched","path":"src/main.rs","bytes":256,"edits":2,"sha256":"0123456789abcdef0123456789abcdef"}}});
        let (label, preview) = operation_output_preview(&edit).unwrap();
        assert_eq!(label, "Edit summary");
        assert!(preview.contains("patched src/main.rs") && preview.contains("2 edits"));
        assert!(preview.contains("SHA256 0123456789ab"));
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
    fn goal_time_uses_utc_and_persisted_start_time() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(-1), "1969-12-31 23:59:59 UTC");
        assert_eq!(format_utc(1_704_067_200), "2024-01-01 00:00:00 UTC");
        assert_eq!(format_clock_utc(3_723), "01:02:03 UTC");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");

        let terminal = Terminal::default();
        terminal.set_task_started_at(Some(100));
        assert_eq!(
            terminal.completion_timing(165),
            "Completed 1970-01-01 00:02:45 UTC · Worked for 1m 05s"
        );
        terminal.set_task_started_at(None);
        assert_eq!(
            terminal.completion_timing(0),
            "Completed 1970-01-01 00:00:00 UTC"
        );
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
    fn editor_wraps_multiline_unicode_and_scrolls_to_the_caret() {
        let text: Vec<_> = "abcd日本\nlast".chars().collect();
        let view = editor_view(&text, text.len(), 4, 2);
        assert_eq!(view.rows, vec!["last", ""]);
        assert_eq!(view.caret_row, 1);
        assert_eq!(view.caret_column, 0);

        let resized = editor_view(&text, 4, 3, 4);
        assert_eq!(resized.rows, vec!["abc", "d日", "本", "las"]);
        assert_eq!(resized.caret_row, 1);
        assert_eq!(resized.caret_column, 1);
    }

    #[test]
    fn resize_events_reflow_prompt_and_menu_for_the_new_terminal_dimensions() {
        let initial = TerminalSize {
            columns: 80,
            rows: 24,
        };
        let resize = Event::Resize(132, 48);
        let expanded = TerminalSize::from_event(&resize).unwrap();
        assert_eq!(
            expanded,
            TerminalSize {
                columns: 132,
                rows: 48
            }
        );

        let narrow = input_layout(initial, "> ", true);
        let wide = input_layout(expanded, "> ", true);
        assert!(wide.composer_width > narrow.composer_width);
        assert!(wide.available > narrow.available);
        assert_eq!(wide.max_input_rows, 6);
        assert!(menu_rows(expanded.rows, 40) > menu_rows(initial.rows, 40));

        let tiny = input_layout(
            TerminalSize {
                columns: 40,
                rows: 8,
            },
            "> ",
            true,
        );
        assert_eq!(tiny.max_input_rows, 1);
        let draft: Vec<_> = "first line that wraps æ—¥æœ¬\nsecond line"
            .chars()
            .collect();
        let compact = editor_view(&draft, draft.len(), narrow.available, narrow.max_input_rows);
        let expanded = editor_view(&draft, draft.len(), wide.available, wide.max_input_rows);
        assert!(expanded.rows.len() <= compact.rows.len());
        assert!(
            expanded
                .rows
                .iter()
                .all(|row| row.width() <= wide.available)
        );
    }

    #[test]
    fn activity_view_refreshes_after_a_height_only_resize() {
        let before = activity_view_key(100, 24, "Working".into(), Some("context".into()));
        let after = activity_view_key(100, 50, "Working".into(), Some("context".into()));
        assert_ne!(before, after);
    }

    #[test]
    fn vertical_cursor_moves_between_wrapped_and_explicit_lines_at_the_same_column() {
        let text: Vec<_> = "abcd\nxy\n1234".chars().collect();
        let (middle, column) = editor_vertical_move(&text, 2, 4, KeyCode::Down, None).unwrap();
        assert_eq!((middle, column), (7, 2));
        let (last, column) =
            editor_vertical_move(&text, middle, 4, KeyCode::Down, Some(column)).unwrap();
        assert_eq!((last, column), (10, 2));
        assert_eq!(
            editor_vertical_move(&text, last, 4, KeyCode::Up, Some(column)),
            Some((7, 2))
        );
        assert_eq!(editor_vertical_move(&text, 0, 4, KeyCode::Up, None), None);
    }

    #[test]
    fn paste_normalizes_line_endings_and_removes_control_sequences() {
        assert_eq!(
            normalize_paste("first\r\nsecond\rthird\u{1b}[2J"),
            "first\nsecond\nthird[2J"
        );
        assert_eq!(
            normalize_paste("if ready:\r\n\twork()\n12345\tX\u{202e}\u{1b}"),
            "if ready:\n    work()\n12345   X"
        );
        assert!(
            !normalize_paste("\ttext\u{202e}\u{1b}")
                .chars()
                .any(char::is_control)
        );
    }

    #[test]
    fn typed_insertions_share_the_byte_limit_and_count_utf8_bytes() {
        let mut text = vec!['x'; MAX_INPUT_BYTES - 3];
        let mut caret = text.len();
        let mut bytes = input_bytes(&text);
        assert!(insert_input(&mut text, &mut caret, '日', &mut bytes));
        assert_eq!(bytes, MAX_INPUT_BYTES);
        assert!(!insert_input(&mut text, &mut caret, 'a', &mut bytes));
        assert_eq!(caret, MAX_INPUT_BYTES - 2);
        assert_eq!(text.len(), caret);

        assert!(can_insert_input(MAX_INPUT_BYTES - 1, 1));
        assert!(!can_insert_input(MAX_INPUT_BYTES, 1));
        assert!(!can_insert_input(MAX_INPUT_BYTES - 2, '日'.len_utf8()));
    }

    #[test]
    fn home_and_end_move_to_current_logical_line_boundaries() {
        let text: Vec<_> = "first\n日本 second\nthird".chars().collect();
        let second_start = "first\n".chars().count();
        let second_end = second_start + "日本 second".chars().count();
        let caret = second_start + 4;
        assert_eq!(logical_line_start(&text, caret), second_start);
        assert_eq!(logical_line_end(&text, caret), second_end);
        assert_eq!(logical_line_end(&text, second_end), second_end);
        assert_eq!(logical_line_start(&text, second_end + 1), second_end + 1);
        assert_eq!(logical_line_start(&text, usize::MAX), second_end + 1);
        assert_eq!(logical_line_end(&text, usize::MAX), text.len());
    }

    #[test]
    fn shrinking_a_composer_keeps_its_full_painted_footprint_for_cleanup() {
        assert_eq!(composer_draw_rows(8, 3), 8);
        assert_eq!(composer_draw_rows(3, 8), 8);
        assert_eq!(composer_draw_rows(3, 3), 3);
    }

    #[test]
    fn composer_cursor_accounts_for_the_painted_margin_and_input_viewport() {
        let prefix = "> ";
        let margin = 2;
        let width = 40;
        let area = ratatui::layout::Rect::new(0, 0, width - 4, 5);
        let input = crate::widgets::composer_input_area(prefix, area);
        let text: Vec<_> = "hello 日本語 world\nsecond line".chars().collect();
        for caret in 0..=text.len() {
            let view = editor_view(&text, caret, input.width as usize, input.height as usize);
            let (buffer, origin) = crate::widgets::composer(
                prefix,
                &view.rows,
                "model / high",
                area.width,
                &crate::ui::Palette::default(),
            );
            assert_eq!(origin, input.x);
            assert!(view.caret_row < input.height as usize);
            assert!(view.caret_column < input.width as usize);
            if !view.rows[view.caret_row].is_empty() {
                assert_ne!(buffer[(origin, 1 + view.caret_row as u16)].symbol(), " ");
            }
            assert_eq!(composer_input_column(origin, margin), input.x as usize + 2);
            assert!(composer_input_column(origin, margin) + view.caret_column < width as usize);
        }
    }

    #[test]
    fn input_history_restores_unsent_unicode_drafts_and_exact_carets() {
        let entries = vec!["first request".into(), "last 日本語 request".into()];
        let mut history = InputHistory::new(&entries);
        let original: Vec<_> = "unfinished 🦊\nnext step".chars().collect();
        let mut text = original.clone();
        let mut caret = 5;
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        assert_eq!((&text, caret), (&original, 5));
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        assert_eq!(text.iter().collect::<String>(), entries[1]);
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        assert_eq!(text.iter().collect::<String>(), entries[0]);
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        assert_eq!((&text, caret), (&original, 5));
        text.insert(caret, '!');
        caret += 1;
        let edited = text.clone();
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        assert_eq!((text, caret), (edited, 6));
        assert!(history.draft.is_none());
    }

    #[test]
    fn clearing_history_navigation_cannot_resurrect_a_discarded_draft() {
        let entries = vec!["previous request".into()];
        let mut history = InputHistory::new(&entries);
        let mut text: Vec<_> = "discard this".chars().collect();
        let mut caret = text.len();
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        text.clear();
        caret = 0;
        history.reset();
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        assert!(text.is_empty());
        assert_eq!(caret, 0);
        history.navigate(KeyCode::Up, &mut text, &mut caret);
        history.navigate(KeyCode::Down, &mut text, &mut caret);
        assert!(text.is_empty());
        assert_eq!(caret, 0);
        let mut empty = InputHistory::new(&[]);
        text = vec!['🦊'];
        caret = 1;
        empty.navigate(KeyCode::Up, &mut text, &mut caret);
        empty.navigate(KeyCode::Down, &mut text, &mut caret);
        assert_eq!((text, caret), (vec!['🦊'], 1));
        assert!(empty.draft.is_none());
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
    fn selecting_a_root_goal_command_leaves_space_for_followup_text() {
        for command in ["/goal", "/contract"] {
            assert_eq!(selected_command_draft(command), format!("{command} "));
        }
        assert_eq!(selected_command_draft("/goal add"), "/goal add");

        let terminal = Terminal::default();
        terminal.set_command_draft("/goal");
        let (draft, caret) = terminal.take_input_draft(&terminal.input_prefix(), false);
        assert_eq!(draft.iter().collect::<String>(), "/goal ");
        assert_eq!(caret, draft.len());
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
