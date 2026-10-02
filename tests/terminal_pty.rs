//! Exercise the real Windows terminal input and rendering paths, without a model call.
#![cfg(windows)]

use anyhow::{Context, Result, bail, ensure};
use arun::storage::Store;
use std::ffi::OsStr;
use std::fs;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Hidden ConPTY host. The dedicated output reader also prevents the pseudo console
/// from blocking while the application paints or while ClosePseudoConsole drains it.
struct Pty {
    workspace: std::path::PathBuf,
    console: HPCON,
    input: HANDLE,
    process: HANDLE,
    reader: Option<JoinHandle<()>>,
    output: Arc<Mutex<Vec<u8>>>,
    initial_size: (usize, usize),
    resizes: Vec<(usize, usize, usize)>,
}

impl Pty {
    fn start(workspace: &std::path::Path, width: usize, height: usize) -> Result<Self> {
        unsafe {
            let mut input_read = null_mut();
            let mut input_write = null_mut();
            let mut output_read = null_mut();
            let mut output_write = null_mut();
            ensure!(
                CreatePipe(&mut input_read, &mut input_write, null(), 0) != 0,
                "input pipe: {}",
                std::io::Error::last_os_error()
            );
            if CreatePipe(&mut output_read, &mut output_write, null(), 0) == 0 {
                CloseHandle(input_read);
                CloseHandle(input_write);
                bail!("output pipe: {}", std::io::Error::last_os_error());
            }
            let mut console = 0;
            let result = CreatePseudoConsole(
                COORD {
                    X: width as i16,
                    Y: height as i16,
                },
                input_read,
                output_write,
                0,
                &mut console,
            );
            CloseHandle(input_read);
            CloseHandle(output_write);
            if result < 0 {
                CloseHandle(input_write);
                CloseHandle(output_read);
                bail!("CreatePseudoConsole HRESULT {result:#x}");
            }
            let output = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&output);
            let reader_handle = output_read as usize;
            let reader = thread::spawn(move || {
                let handle = reader_handle as HANDLE;
                let mut buffer = [0_u8; 8192];
                loop {
                    let mut count = 0;
                    if ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut count,
                        null_mut(),
                    ) == 0
                        || count == 0
                    {
                        break;
                    }
                    captured
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buffer[..count as usize]);
                }
                CloseHandle(handle);
            });
            let mut pty = Self {
                workspace: workspace.to_owned(),
                console,
                input: input_write,
                process: null_mut(),
                reader: Some(reader),
                output,
                initial_size: (width, height),
                resizes: Vec::new(),
            };
            let mut bytes = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes);
            let mut attributes = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
            let attribute_list = attributes.as_mut_ptr().cast();
            ensure!(
                InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut bytes) != 0,
                "attribute list: {}",
                std::io::Error::last_os_error()
            );
            let creation = (|| -> Result<()> {
                ensure!(
                    UpdateProcThreadAttribute(
                        attribute_list,
                        0,
                        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                        console as *const _,
                        size_of::<HPCON>(),
                        null_mut(),
                        null()
                    ) != 0,
                    "pseudo console attribute: {}",
                    std::io::Error::last_os_error()
                );
                let mut startup: STARTUPINFOEXW = zeroed();
                startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
                // Do not inherit the cargo runner's redirected standard streams.
                startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                startup.lpAttributeList = attribute_list;
                let mut process: PROCESS_INFORMATION = zeroed();
                let executable = wide(env!("CARGO_BIN_EXE_arun"));
                let mut command = wide(format!("\"{}\"", env!("CARGO_BIN_EXE_arun")));
                let cwd = wide(workspace);
                // Test colors independently of the invoking shell's NO_COLOR.
                // This environment belongs only to the hidden child process.
                let mut environment: Vec<_> = std::env::vars_os()
                    .filter(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case("NO_COLOR"))
                    .collect();
                environment.sort_by_key(|(key, _)| key.to_string_lossy().to_lowercase());
                let mut environment_block = Vec::<u16>::new();
                for (key, value) in environment {
                    environment_block.extend(key.encode_wide());
                    environment_block.push('=' as u16);
                    environment_block.extend(value.encode_wide());
                    environment_block.push(0);
                }
                environment_block.push(0);
                ensure!(
                    CreateProcessW(
                        executable.as_ptr(),
                        command.as_mut_ptr(),
                        null(),
                        null(),
                        0,
                        EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                        environment_block.as_ptr().cast(),
                        cwd.as_ptr(),
                        &startup.StartupInfo,
                        &mut process
                    ) != 0,
                    "CreateProcessW: {}",
                    std::io::Error::last_os_error()
                );
                pty.process = process.hProcess;
                CloseHandle(process.hThread);
                Ok(())
            })();
            DeleteProcThreadAttributeList(attribute_list);
            creation?;
            Ok(pty)
        }
    }

    fn send_keys(&self, text: &str) -> Result<()> {
        // ConPTY requests Win32 input mode (DECSET 9001). Model a Windows
        // Terminal host by sending key packets; bare UTF-8 instead invokes
        // conhost's legacy key synthesis, which can lose clipboard delimiters
        // and turn combining characters into Alt+numpad key-up events.
        // Windows Terminal's actual clipboard packet format is documented in:
        // https://github.com/microsoft/terminal/issues/17656
        let mut packets = String::new();
        for character in text.chars() {
            let (vk, flags) = match character {
                '\r' => (13, 0),
                '\x1b' => (27, 0),
                '\x01'..='\x1a' => (64 + character as u32, 8),
                _ => (0, 0),
            };
            let mut units = [0_u16; 2];
            for &unit in character.encode_utf16(&mut units).iter() {
                use std::fmt::Write as _;
                write!(packets, "\x1b[{vk};0;{unit};1;{flags};1_")?;
            }
        }
        self.write_input(packets.as_bytes())
    }

    fn send_key(&self, vk: u32, unicode: u16, control_state: u32) -> Result<()> {
        self.write_input(format!("\x1b[{vk};0;{unicode};1;{control_state};1_").as_bytes())
    }

    fn send_clipboard(&self, text: &str) -> Result<()> {
        let mut packets = String::new();
        for unit in format!("\x1b[200~{text}\x1b[201~").encode_utf16() {
            use std::fmt::Write as _;
            write!(packets, "\x1b[0;0;{unit};1;0;1_")?;
        }
        self.write_input(packets.as_bytes())
    }

    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written = 0;
            unsafe {
                ensure!(
                    WriteFile(
                        self.input,
                        bytes[offset..].as_ptr(),
                        (bytes.len() - offset) as u32,
                        &mut written,
                        null_mut()
                    ) != 0,
                    "terminal input: {}",
                    std::io::Error::last_os_error()
                );
            }
            ensure!(written != 0, "terminal input wrote zero bytes");
            offset += written as usize;
        }
        Ok(())
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<()> {
        let previous_output = self.output.lock().unwrap().len();
        let result = unsafe {
            ResizePseudoConsole(
                self.console,
                COORD {
                    X: width as i16,
                    Y: height as i16,
                },
            )
        };
        ensure!(result >= 0, "ResizePseudoConsole HRESULT {result:#x}");
        self.resizes.push((previous_output, width, height));
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.output.lock().unwrap().len() == previous_output {
            ensure!(
                Instant::now() < deadline,
                "resize produced no terminal redraw"
            );
            thread::sleep(Duration::from_millis(30));
        }
        // ConPTY can split one repaint over several output chunks.
        thread::sleep(Duration::from_millis(250));
        Ok(())
    }

    fn screen(&self) -> Screen {
        let bytes = self.output.lock().unwrap().clone();
        Screen::decode(
            &String::from_utf8_lossy(&bytes),
            self.initial_size,
            &self.resizes,
        )
    }

    fn wait_screen(
        &self,
        description: &str,
        predicate: impl Fn(&Screen) -> bool,
    ) -> Result<Screen> {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let screen = self.screen();
            if predicate(&screen) {
                return Ok(screen);
            }
            if Instant::now() >= deadline {
                bail!(
                    "Timed out waiting for {description}. Screen:\n{}\nRaw:\n{}",
                    screen.text(),
                    String::from_utf8_lossy(&self.output.lock().unwrap())
                );
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn finish(&self) -> Result<()> {
        unsafe {
            ensure!(
                WaitForSingleObject(self.process, 5000) == WAIT_OBJECT_0,
                "terminal did not exit"
            );
            let mut code = 0;
            ensure!(
                GetExitCodeProcess(self.process, &mut code) != 0,
                "read terminal exit code"
            );
            ensure!(code == 0, "terminal exited with {code}");
        }
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // If a paste regression unexpectedly submits text, stop any fixture task
        // before its temporary workspace disappears. This also cleans up on panic.
        if let Ok(mut store) = Store::open(&self.workspace.join(".arun"))
            && let Ok(runs) = store.runs()
        {
            for run in runs {
                if !run.is_terminal() {
                    let _ = store.state(
                        &run.id,
                        "cancelled",
                        serde_json::json!({"source":"pty_fixture_cleanup"}),
                    );
                }
            }
        }
        unsafe {
            if !self.process.is_null() {
                if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                    TerminateProcess(self.process, 1);
                    WaitForSingleObject(self.process, 5000);
                }
                CloseHandle(self.process);
            }
            CloseHandle(self.input);
            ClosePseudoConsole(self.console);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Small screen decoder for assertions about final cell placement, rather than
/// matching stale text anywhere in a sequence of repaint escape codes.
struct Screen {
    cells: Vec<Vec<String>>,
    row: usize,
    column: usize,
    saved: (usize, usize),
    escape: EscapeState,
}

enum EscapeState {
    Ground,
    Escape,
    Csi(String),
    Osc,
    OscEscape,
    Charset,
}

impl Screen {
    fn decode(
        text: &str,
        (width, height): (usize, usize),
        resizes: &[(usize, usize, usize)],
    ) -> Self {
        let mut screen = Self {
            cells: vec![vec![String::new(); width]; height],
            row: 0,
            column: 0,
            saved: (0, 0),
            escape: EscapeState::Ground,
        };
        let mut resize = 0;
        for (offset, character) in text.char_indices() {
            while let Some(&(at, columns, rows)) = resizes.get(resize) {
                if offset < at {
                    break;
                }
                screen.resize(columns, rows);
                resize += 1;
            }
            screen.feed(character);
        }
        for &(_, columns, rows) in &resizes[resize..] {
            screen.resize(columns, rows);
        }
        screen
    }

    fn resize(&mut self, width: usize, height: usize) {
        // Keep the cursor visible when a console shrinks vertically. Future
        // ConPTY paint sequences apply to this resized state, never to a replay
        // of earlier output using the new dimensions.
        if self.row >= height {
            let removed = self.row - height + 1;
            self.cells.drain(..removed.min(self.cells.len()));
            self.row -= removed;
        }
        self.cells
            .resize_with(height, || vec![String::new(); width]);
        for line in &mut self.cells {
            line.resize(width, String::new());
        }
        self.column = self.column.min(width - 1);
        self.saved.0 = self.saved.0.min(height - 1);
        self.saved.1 = self.saved.1.min(width - 1);
    }

    fn feed(&mut self, character: char) {
        let state = std::mem::replace(&mut self.escape, EscapeState::Ground);
        match state {
            EscapeState::Ground => match character {
                '\x1b' => self.escape = EscapeState::Escape,
                '\r' => self.column = 0,
                '\n' => self.line_feed(),
                '\x08' => self.column = self.column.saturating_sub(1),
                '\t' => self.column = ((self.column / 8 + 1) * 8).min(self.cells[0].len() - 1),
                value if !value.is_control() => self.put(value),
                _ => {}
            },
            EscapeState::Escape => match character {
                '[' => self.escape = EscapeState::Csi(String::new()),
                ']' => self.escape = EscapeState::Osc,
                '7' => self.saved = (self.row, self.column),
                '8' => (self.row, self.column) = self.saved,
                '(' | ')' => self.escape = EscapeState::Charset,
                _ => {}
            },
            EscapeState::Csi(mut parameters) => {
                if ('@'..='~').contains(&character) {
                    self.csi(&parameters, character);
                } else {
                    parameters.push(character);
                    self.escape = EscapeState::Csi(parameters);
                }
            }
            EscapeState::Osc => match character {
                '\x07' => {}
                '\x1b' => self.escape = EscapeState::OscEscape,
                _ => self.escape = EscapeState::Osc,
            },
            EscapeState::OscEscape => {
                if character != '\\' {
                    self.escape = EscapeState::Osc;
                }
            }
            EscapeState::Charset => {}
        }
    }

    fn line_feed(&mut self) {
        self.row += 1;
        if self.row >= self.cells.len() {
            self.cells.remove(0);
            self.cells.push(vec![String::new(); self.cells[0].len()]);
            self.row = self.cells.len() - 1;
        }
    }

    fn put(&mut self, character: char) {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            if self.column > 0 {
                self.cells[self.row][self.column - 1].push(character);
            }
            return;
        }
        let columns = self.cells[0].len();
        if self.column + width > columns {
            self.column = 0;
            self.line_feed();
        }
        self.cells[self.row][self.column] = character.to_string();
        for offset in 1..width {
            self.cells[self.row][self.column + offset] = "\0".into();
        }
        self.column += width;
    }

    fn csi(&mut self, parameters: &str, command: char) {
        let values: Vec<usize> = parameters
            .split(';')
            .map(|value| value.parse().unwrap_or(0))
            .collect();
        let first = values.first().copied().unwrap_or(0);
        let amount = first.max(1);
        let max_row = self.cells.len() - 1;
        let max_column = self.cells[0].len() - 1;
        match command {
            'H' | 'f' => {
                self.row = first.max(1).saturating_sub(1).min(max_row);
                self.column = values
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1)
                    .min(max_column);
            }
            'A' => self.row = self.row.saturating_sub(amount),
            'B' => self.row = (self.row + amount).min(max_row),
            'C' => self.column = (self.column + amount).min(max_column),
            'D' => self.column = self.column.saturating_sub(amount),
            'G' => self.column = amount.saturating_sub(1).min(max_column),
            'd' => self.row = amount.saturating_sub(1).min(max_row),
            'J' => match first {
                2 | 3 => self
                    .cells
                    .iter_mut()
                    .for_each(|line| line.fill(String::new())),
                0 => {
                    self.cells[self.row][self.column.min(max_column)..].fill(String::new());
                    for line in &mut self.cells[self.row + 1..] {
                        line.fill(String::new());
                    }
                }
                _ => {}
            },
            'K' => match first {
                0 => self.cells[self.row][self.column.min(max_column)..].fill(String::new()),
                1 => self.cells[self.row][..=self.column.min(max_column)].fill(String::new()),
                2 => self.cells[self.row].fill(String::new()),
                _ => {}
            },
            's' => self.saved = (self.row, self.column),
            'u' => (self.row, self.column) = self.saved,
            _ => {}
        }
    }

    fn lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|line| {
                line.iter()
                    .map(|cell| {
                        if cell.is_empty() {
                            " "
                        } else if cell == "\0" {
                            ""
                        } else {
                            cell
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn text(&self) -> String {
        self.lines().join("\n")
    }
}

#[test]
fn actual_windows_terminal_preserves_multiline_drafts_and_resizes_command_picker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&serde_json::json!({
            "provider":"custom", "model":"fixture",
            "endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},
            "write":false,"image":null,"previous_run":null
        }))?,
    )?;
    let mut terminal =
        Pty::start(directory.path(), 96, 30).context("start real Windows terminal")?;
    terminal.wait_screen("initial composer", |screen| {
        screen
            .text()
            .contains("fixture · default effort · review only")
    })?;
    terminal.send_clipboard("first line\r\n  日本語 e\u{301} 🙂\r\nlast line")?;
    terminal.wait_screen("three distinct pasted lines", |screen| {
        if !screen
            .text()
            .contains("fixture · default effort · review only")
        {
            return false;
        }
        let lines = screen.lines();
        let first = lines.iter().position(|line| line.contains("first line"));
        let middle = lines
            .iter()
            .position(|line| line.contains("日本語 e\u{301} 🙂"));
        let last = lines.iter().position(|line| line.contains("last line"));
        matches!((first, middle, last), (Some(a), Some(b), Some(c)) if a < b && b < c)
    })?;
    ensure!(
        Store::open(&root)?.runs()?.is_empty(),
        "paste submitted a task before Enter"
    );
    terminal.send_key(13, 13, 0x10)?; // Native Shift+Enter.
    terminal.send_keys("SHIFT_ENTER_LINE")?;
    terminal.send_key(13, 10, 0x08)?; // Native Ctrl+J represented by console VK_RETURN.
    terminal.send_keys("CTRL_J_LINE")?;
    terminal.wait_screen("native multiline keyboard shortcuts", |screen| {
        let lines = screen.lines();
        matches!((lines.iter().position(|line| line.contains("last line")), lines.iter().position(|line| line.contains("SHIFT_ENTER_LINE")), lines.iter().position(|line| line.contains("CTRL_J_LINE"))), (Some(a), Some(b), Some(c)) if a < b && b < c)
            && screen.text().contains("fixture · default effort · review only")
    })?;
    ensure!(
        Store::open(&root)?.runs()?.is_empty(),
        "newline shortcuts submitted a task"
    );
    terminal.resize(38, 14)?;
    terminal.wait_screen("narrow composer retaining paste", |screen| {
        screen.text().contains("last line")
    })?;
    terminal.send_key(38, 0, 0)?; // Up/Down navigate multiline visual rows.
    terminal.send_key(40, 0, 0)?;
    terminal.send_keys("\x15")?;
    terminal.send_clipboard(&format!("WRAP_BEGIN_{}_WRAP_END", "a".repeat(90)))?;
    terminal.wait_screen("long draft wrapping over visual rows", |screen| {
        let lines = screen.lines();
        matches!((lines.iter().position(|line| line.contains("WRAP_BEGIN_")), lines.iter().position(|line| line.contains("_WRAP_END"))), (Some(first), Some(last)) if last > first)
    })?;
    terminal.resize(96, 30)?;
    terminal.wait_screen("wide composer retaining long draft", |screen| {
        screen.text().contains("WRAP_BEGIN_") && screen.text().contains("_WRAP_END")
    })?;
    terminal.send_keys("\x15")?; // Ctrl+U clears the draft.
    terminal.send_keys("/")?;
    terminal.wait_screen("slash picker", |screen| {
        screen.text().contains("/goal") && screen.text().contains("/model")
    })?;
    terminal.send_key(35, 0, 0)?; // End exposes the last entry of the complete catalog.
    terminal.wait_screen("last slash command is reachable", |screen| {
        screen.text().contains("Slash commands (39)") && screen.text().contains("/quit")
    })?;
    terminal.send_key(36, 0, 0)?;
    terminal.send_keys("reason")?;
    terminal.wait_screen("filtered slash picker", |screen| {
        screen.text().contains("/reasoning") && !screen.text().contains("/goal")
    })?;
    terminal.resize(38, 14)?;
    terminal.wait_screen("resized filtered picker", |screen| {
        screen.text().contains("/reasoning") && !screen.text().contains("/goal")
    })?;
    terminal.send_key(27, 27, 0)?;
    terminal.send_keys("\x15\x04")?; // Clear draft, Ctrl+D exits idle session.
    terminal.finish()?;
    drop(terminal);
    ensure!(
        Store::open(&root)?.runs()?.is_empty(),
        "editing or selecting a command created a task"
    );
    Ok(())
}

#[test]
fn actual_windows_terminal_preserves_raw_vt_host_clipboard() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&serde_json::json!({
            "provider":"custom", "model":"fixture",
            "endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},
            "write":false,"image":null,"previous_run":null
        }))?,
    )?;
    let terminal = Pty::start(directory.path(), 96, 30)?;
    terminal.wait_screen("raw VT host composer", |screen| {
        screen
            .text()
            .contains("fixture · default effort · review only")
    })?;
    // A host without Win32 input support ignores DECSET 9001 and sends raw VT.
    // This is a distinct transport path from Windows Terminal's UTF-16 packets.
    terminal
        .write_input("\x1b[200~RAW_FIRST\r\n日本語 e\u{301}\r\nRAW_LAST\x1b[201~".as_bytes())?;
    terminal.wait_screen("raw VT multiline clipboard", |screen| {
        let lines = screen.lines();
        let first = lines.iter().position(|line| line.contains("RAW_FIRST"));
        let middle = lines
            .iter()
            .position(|line| line.contains("日本語 e\u{301}"));
        let last = lines.iter().position(|line| line.contains("RAW_LAST"));
        screen
            .text()
            .contains("fixture · default effort · review only")
            && matches!((first, middle, last), (Some(a), Some(b), Some(c)) if a < b && b < c)
    })?;
    ensure!(
        Store::open(&root)?.runs()?.is_empty(),
        "raw VT paste submitted a task before Enter"
    );
    terminal.write_input(b"\x15\x04")?;
    terminal.finish()?;
    drop(terminal);
    ensure!(
        Store::open(&root)?.runs()?.is_empty(),
        "raw VT editing created a task"
    );
    Ok(())
}

#[test]
fn actual_windows_follow_keeps_output_live_inside_steering_and_slash_picker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "deterministic live terminal fixture",
        directory.path(),
        "custom",
        serde_json::json!([]),
        serde_json::json!({}),
        "",
    )?;
    store.state(&run.id, "running", serde_json::json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        serde_json::json!({"program":"fixture"}),
        true,
    )?;
    // An exclusive lock represents a live worker. The fixture only appends events;
    // no process, model, mutation, or acceptance is actually executed.
    let runner = fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(format!("run-{}.lock", run.id)))?;
    fs2::FileExt::lock_exclusive(&runner)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&serde_json::json!({
            "provider":"custom", "model":"fixture",
            "endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},
            "write":false,"image":null,"previous_run":run.id
        }))?,
    )?;
    let terminal = Pty::start(directory.path(), 110, 35)?;
    terminal.wait_screen("saved task composer", |screen| {
        screen
            .text()
            .contains("fixture · default effort · review only")
    })?;
    // '/resume' from the picker selects a command; it takes a second Enter to run.
    terminal.send_keys("/resume\r")?;
    terminal.wait_screen("selected resume command", |screen| {
        screen.text().contains("/resume")
    })?;
    terminal.send_key(13, 13, 0)?;
    terminal.wait_screen("active follower", |screen| {
        screen.text().contains("Type/paste to steer")
    })?;
    terminal.wait_screen("visible live clock and elapsed time", |screen| {
        use chrono::Timelike;
        screen.lines().iter().any(|line| {
            let Some((_, clock)) = line.split_once(" elapsed · ") else {
                return false;
            };
            let Some(clock) = clock.split_once(" UTC").map(|(time, _)| time) else {
                return false;
            };
            let Ok(time) = chrono::NaiveTime::parse_from_str(clock, "%H:%M:%S") else {
                return false;
            };
            let difference = (i64::from(time.num_seconds_from_midnight())
                - i64::from(chrono::Utc::now().num_seconds_from_midnight()))
            .abs();
            difference.min(86400 - difference) <= 10
        })
    })?;
    terminal.send_keys("x")?;
    terminal.wait_screen("active steering composer", |screen| {
        screen.text().contains("Steer this task")
    })?;
    store.event(
        &run.id,
        "operation.output",
        serde_json::json!({"id":operation.id,"text":"LIVE_WHILE_TYPING\n"}),
    )?;
    terminal.wait_screen("live output while draft is open", |screen| {
        screen.text().contains("LIVE_WHILE_TYPING")
    })?;
    store.event(
        &run.id,
        "operation.output",
        serde_json::json!({"id":operation.id,"text":"","truncated":true}),
    )?;
    terminal.wait_screen("empty output cap event shows artifact recovery", |screen| {
        let text = screen.text();
        text.contains("Output preview")
            && text.contains("display limit")
            && text.contains("output artifact")
    })?;
    terminal.send_keys("\x15/")?;
    terminal.wait_screen("nested steering slash picker", |screen| {
        screen.text().contains("/goal") && screen.text().contains("/model")
    })?;
    store.event(
        &run.id,
        "operation.output",
        serde_json::json!({"id":operation.id,"text":"LIVE_INSIDE_PICKER\n"}),
    )?;
    terminal.wait_screen("live output while slash picker is open", |screen| {
        screen.text().contains("LIVE_INSIDE_PICKER") && screen.text().contains("/goal")
    })?;
    terminal.send_key(67, 3, 8)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.event_count(&run.id, "interrupt.requested")? == 0 {
        ensure!(
            Instant::now() < deadline,
            "Ctrl+C inside slash picker did not request interruption. Screen:\n{}",
            terminal.screen().text()
        );
        thread::sleep(Duration::from_millis(30));
    }
    ensure!(
        store.interrupt_requested(&run.id, arun::interrupt::Scope::Operation, &operation.id)?,
        "picker interruption targeted the wrong operation"
    );
    store.state(
        &run.id,
        "answered",
        serde_json::json!({"summary":"LIVE_TASK_FINISHED"}),
    )?;
    terminal.wait_screen("completion feedback while editing", |screen| {
        screen.text().contains("LIVE_TASK_FINISHED") && screen.text().contains("Task finished")
    })?;
    let finished_at = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "run.answered")
        .unwrap()
        .created_at;
    let expected_time = chrono::DateTime::from_timestamp(finished_at, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();
    terminal.wait_screen("completion timestamp and elapsed time", |screen| {
        let text = screen.text();
        text.contains(&format!("Completed {expected_time}")) && text.contains("Elapsed ")
    })?;
    let output = terminal.output.lock().unwrap().clone();
    let output = String::from_utf8_lossy(&output);
    let colors: std::collections::BTreeSet<_> = output
        .split("\x1b[")
        .filter_map(|suffix| suffix.split_once('m').map(|(parameters, _)| parameters))
        .filter(|parameters| parameters.contains("38;2;") || parameters.contains("38;5;"))
        .collect();
    ensure!(
        colors.len() >= 3,
        "expected multiple actual foreground color roles, got {colors:?}"
    );
    terminal.send_keys("\x15\x04")?;
    terminal.finish()?;
    drop(terminal);
    fs2::FileExt::unlock(&runner)?;
    ensure!(
        store.runs()?.len() == 1,
        "terminal input created a second task"
    );
    ensure!(
        store.event_count(&run.id, "model.started")? == 0,
        "fixture unexpectedly invoked a model"
    );
    Ok(())
}
