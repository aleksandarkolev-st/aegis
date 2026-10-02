//! Terminal input shared by every interactive surface. Windows reads VT bytes
//! rather than console INPUT_RECORDs so bracketed paste remains one event.
use std::io;
#[cfg(windows)]
use std::time::Duration;

pub use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};

#[cfg(not(windows))]
pub use crossterm::event::{poll, read};
#[cfg(windows)]
pub use windows::{InputGuard, poll, read, start};

#[cfg(not(windows))]
pub struct InputGuard;
#[cfg(not(windows))]
impl InputGuard {
    pub(crate) fn owns_input(&self) -> bool {
        false
    }
}
#[cfg(not(windows))]
pub fn start() -> io::Result<InputGuard> {
    Ok(InputGuard)
}

#[cfg(any(windows, test))]
mod parser {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use std::collections::VecDeque;

    const PASTE_LIMIT: usize = 65_536;
    const PASTE_START: &[u8] = b"\x1b[200~";
    const PASTE_END: &[u8] = b"\x1b[201~";

    #[derive(Default)]
    pub(super) struct Parser {
        bytes: Vec<u8>,
        ready: VecDeque<Event>,
        paste: Option<Vec<u8>>,
        paste_tail: VecDeque<u8>,
        paste_bytes: usize,
        marker_keys: Vec<Event>,
        surrogate: Option<u16>,
    }

    impl Parser {
        pub fn feed(&mut self, bytes: &[u8]) {
            self.bytes.extend_from_slice(bytes);
            loop {
                if self.bytes.is_empty() {
                    break;
                }
                if self.paste.is_some() {
                    // Conhost may encode pasted characters as Win32 key
                    // packets. Decode those characters without interpreting
                    // clipboard escape sequences as editor commands.
                    if self.bytes.starts_with(b"\x1b[") {
                        match sequence_end(&self.bytes) {
                            None if self.bytes.len() < 128 => break,
                            Some(end) if self.bytes[end] == b'_' => {
                                let packet = self.bytes[2..end].to_vec();
                                self.bytes.drain(..=end);
                                self.win32_key(&packet, true);
                                continue;
                            }
                            _ => {}
                        }
                    } else if self.bytes == b"\x1b" {
                        break;
                    }
                    let byte = self.bytes.remove(0);
                    self.paste_byte(byte);
                    continue;
                }
                if self.bytes.starts_with(PASTE_START) {
                    self.bytes.drain(..PASTE_START.len());
                    self.begin_paste();
                    continue;
                }
                match self.parse_next() {
                    Parse::Wait => break,
                    Parse::Skip(count) => {
                        self.bytes.drain(..count);
                    }
                    Parse::Key(count, key) => {
                        self.bytes.drain(..count);
                        self.accept_key(key);
                    }
                    Parse::Win32(count, packet) => {
                        self.bytes.drain(..count);
                        self.win32_key(&packet, false);
                    }
                    Parse::Event(count, event) => {
                        self.bytes.drain(..count);
                        self.flush_marker();
                        self.ready.push_back(event);
                    }
                }
            }
        }

        pub fn next(&mut self) -> Option<Event> {
            self.ready.pop_front()
        }
        pub fn has_event(&self) -> bool {
            !self.ready.is_empty()
        }
        pub fn escape_pending(&self) -> bool {
            self.paste.is_none() && (self.bytes == b"\x1b" || self.marker_keys.len() == 1)
        }
        pub fn expire_escape(&mut self) {
            if self.bytes == b"\x1b" && self.paste.is_none() {
                self.bytes.clear();
                self.ready.push_back(Event::Key(KeyCode::Esc.into()));
            }
            self.flush_marker();
        }

        fn begin_paste(&mut self) {
            self.flush_marker();
            self.paste = Some(Vec::new());
            self.paste_tail.clear();
            self.paste_bytes = 0;
        }
        fn paste_byte(&mut self, byte: u8) {
            let paste = self.paste.as_mut().unwrap();
            self.paste_bytes += 1;
            if paste.len() < PASTE_LIMIT + PASTE_END.len() + 1 {
                paste.push(byte);
            }
            self.paste_tail.push_back(byte);
            if self.paste_tail.len() > PASTE_END.len() {
                self.paste_tail.pop_front();
            }
            if self
                .paste_tail
                .iter()
                .copied()
                .eq(PASTE_END.iter().copied())
            {
                let mut paste = self.paste.take().unwrap();
                let length = self.paste_bytes - PASTE_END.len();
                paste.truncate(length.min(PASTE_LIMIT + 1));
                self.ready
                    .push_back(Event::Paste(String::from_utf8_lossy(&paste).into_owned()));
                self.paste_tail.clear();
            }
        }
        fn flush_marker(&mut self) {
            self.ready.extend(self.marker_keys.drain(..));
        }
        fn accept_key(&mut self, key: KeyEvent) {
            if key.kind == KeyEventKind::Release {
                return;
            }
            if key.modifiers.is_empty()
                || matches!(key.code, KeyCode::Char(_)) && key.modifiers == KeyModifiers::SHIFT
            {
                let byte = match key.code {
                    KeyCode::Esc => Some(27),
                    KeyCode::Char(character) if character.is_ascii() => Some(character as u8),
                    _ => None,
                };
                if byte.is_some_and(|byte| PASTE_START.get(self.marker_keys.len()) == Some(&byte)) {
                    self.marker_keys.push(Event::Key(key));
                    if self.marker_keys.len() == PASTE_START.len() {
                        self.marker_keys.clear();
                        self.begin_paste();
                    }
                    return;
                }
            }
            self.flush_marker();
            self.ready.push_back(Event::Key(key));
        }

        fn win32_key(&mut self, packet: &[u8], inside_paste: bool) {
            let Some(parameters) = parameters(packet) else {
                return;
            };
            if parameters.len() > 6 {
                return;
            }
            let vk = parameters.first().copied().unwrap_or(0);
            let uc = parameters.get(2).copied().unwrap_or(0);
            let key_down = parameters.get(3).copied().unwrap_or(0) != 0;
            // Classic ConHost emits composed Alt-numpad text on Alt release.
            // Ordinary releases remain ignored so typed letters don't double.
            let alt_code = vk == 18 && !key_down && uc != 0;
            if !key_down && !alt_code {
                return;
            }
            let flags = parameters.get(4).copied().unwrap_or(0);
            let repeat = parameters.get(5).copied().unwrap_or(1).max(1).min(256);
            let mut modifiers = KeyModifiers::NONE;
            if flags & 0x10 != 0 {
                modifiers |= KeyModifiers::SHIFT;
            }
            if flags & 0x0c != 0 {
                modifiers |= KeyModifiers::CONTROL;
            }
            if flags & 0x03 != 0 {
                modifiers |= KeyModifiers::ALT;
            }
            if alt_code {
                modifiers.remove(KeyModifiers::ALT);
            }
            // AltGr produces printable characters, not application shortcuts.
            if flags & 0x09 == 0x09 && uc >= 32 {
                modifiers.remove(KeyModifiers::CONTROL | KeyModifiers::ALT);
            }
            let character = if (0xd800..=0xdbff).contains(&uc) {
                self.surrogate = Some(uc as u16);
                return;
            } else if (0xdc00..=0xdfff).contains(&uc) {
                self.surrogate.take().and_then(|high| {
                    char::from_u32(0x10000 + (((high as u32 - 0xd800) << 10) | (uc - 0xdc00)))
                })
            } else {
                self.surrogate = None;
                char::from_u32(uc)
            };
            if inside_paste {
                if let Some(character) = character {
                    let mut utf8 = [0; 4];
                    for _ in 0..repeat {
                        for byte in character.encode_utf8(&mut utf8).bytes() {
                            if self.paste.is_none() {
                                return;
                            }
                            self.paste_byte(byte);
                        }
                    }
                }
                return;
            }
            let code = match vk {
                8 => KeyCode::Backspace,
                9 => KeyCode::Tab,
                13 if uc == 10 && modifiers.contains(KeyModifiers::CONTROL) => KeyCode::Char('j'),
                13 => KeyCode::Enter,
                27 => KeyCode::Esc,
                33 => KeyCode::PageUp,
                34 => KeyCode::PageDown,
                35 => KeyCode::End,
                36 => KeyCode::Home,
                37 => KeyCode::Left,
                38 => KeyCode::Up,
                39 => KeyCode::Right,
                40 => KeyCode::Down,
                45 => KeyCode::Insert,
                46 => KeyCode::Delete,
                112..=135 => KeyCode::F((vk - 111) as u8),
                _ => {
                    let Some(character) = character.filter(|character| *character != '\0') else {
                        return;
                    };
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && (1..=26).contains(&(character as u32))
                    {
                        KeyCode::Char((b'a' + character as u8 - 1) as char)
                    } else {
                        scalar_key(character as u32).unwrap_or(KeyCode::Char(character))
                    }
                }
            };
            for _ in 0..repeat {
                self.accept_key(KeyEvent::new(code, modifiers));
            }
        }

        fn parse_next(&self) -> Parse {
            let bytes = &self.bytes;
            if bytes[0] != 27 {
                return plain_key(bytes);
            }
            if bytes.len() == 1 {
                return Parse::Wait;
            }
            if bytes[1] == b'[' {
                let Some(end) = sequence_end(bytes) else {
                    return if bytes.len() >= 128 {
                        Parse::Skip(1)
                    } else {
                        Parse::Wait
                    };
                };
                let packet = &bytes[2..end];
                if bytes[end] == b'_' {
                    return Parse::Win32(end + 1, packet.to_vec());
                }
                return csi_key(packet, bytes[end], end + 1);
            }
            if bytes[1] == b'O' {
                if bytes.len() < 3 {
                    return Parse::Wait;
                }
                let code = match bytes[2] {
                    b'A' => KeyCode::Up,
                    b'B' => KeyCode::Down,
                    b'C' => KeyCode::Right,
                    b'D' => KeyCode::Left,
                    b'H' => KeyCode::Home,
                    b'F' => KeyCode::End,
                    b'P'..=b'S' => KeyCode::F(bytes[2] - b'P' + 1),
                    _ => return Parse::Skip(3),
                };
                return Parse::Key(3, code.into());
            }
            match plain_key(&bytes[1..]) {
                Parse::Key(count, mut key) => {
                    key.modifiers |= KeyModifiers::ALT;
                    Parse::Key(count + 1, key)
                }
                Parse::Wait => Parse::Wait,
                _ => Parse::Skip(1),
            }
        }
    }

    enum Parse {
        Wait,
        Skip(usize),
        Key(usize, KeyEvent),
        Win32(usize, Vec<u8>),
        Event(usize, Event),
    }
    fn sequence_end(bytes: &[u8]) -> Option<usize> {
        bytes
            .iter()
            .enumerate()
            .skip(2)
            .find_map(|(index, byte)| (0x40..=0x7e).contains(byte).then_some(index))
    }
    fn parameters(bytes: &[u8]) -> Option<Vec<u32>> {
        std::str::from_utf8(bytes)
            .ok()?
            .split(';')
            .map(|value| {
                if value.is_empty() {
                    Some(0)
                } else {
                    value.split(':').next()?.parse().ok()
                }
            })
            .collect()
    }
    fn modifiers(parameter: u32) -> KeyModifiers {
        let flags = parameter.saturating_sub(1);
        let mut modifiers = KeyModifiers::NONE;
        if flags & 1 != 0 {
            modifiers |= KeyModifiers::SHIFT;
        }
        if flags & 2 != 0 {
            modifiers |= KeyModifiers::ALT;
        }
        if flags & 4 != 0 {
            modifiers |= KeyModifiers::CONTROL;
        }
        if flags & 8 != 0 {
            modifiers |= KeyModifiers::SUPER;
        }
        modifiers
    }
    fn scalar_key(value: u32) -> Option<KeyCode> {
        Some(match value {
            9 => KeyCode::Tab,
            13 => KeyCode::Enter,
            27 => KeyCode::Esc,
            127 => KeyCode::Backspace,
            57344 => KeyCode::Esc,
            57345 => KeyCode::Enter,
            57346 => KeyCode::Tab,
            57347 => KeyCode::Backspace,
            57348 => KeyCode::Insert,
            57349 => KeyCode::Delete,
            57350 => KeyCode::Left,
            57351 => KeyCode::Right,
            57352 => KeyCode::Up,
            57353 => KeyCode::Down,
            57354 => KeyCode::PageUp,
            57355 => KeyCode::PageDown,
            57356 => KeyCode::Home,
            57357 => KeyCode::End,
            57364..=57398 => KeyCode::F((value - 57363) as u8),
            _ => KeyCode::Char(char::from_u32(value)?),
        })
    }
    fn csi_key(packet: &[u8], final_byte: u8, count: usize) -> Parse {
        if packet.is_empty() && matches!(final_byte, b'I' | b'O') {
            return Parse::Event(
                count,
                if final_byte == b'I' {
                    Event::FocusGained
                } else {
                    Event::FocusLost
                },
            );
        }
        let Some(parameters) = parameters(packet) else {
            return Parse::Skip(count);
        };
        let modifier = modifiers(parameters.get(1).copied().unwrap_or(1));
        let code = match final_byte {
            b'A' => KeyCode::Up,
            b'B' => KeyCode::Down,
            b'C' => KeyCode::Right,
            b'D' => KeyCode::Left,
            b'H' => KeyCode::Home,
            b'F' => KeyCode::End,
            b'Z' => return Parse::Key(count, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            b'P'..=b'S' => KeyCode::F(final_byte - b'P' + 1),
            b'u' => {
                let Some(code) = scalar_key(parameters[0]) else {
                    return Parse::Skip(count);
                };
                return Parse::Key(count, KeyEvent::new(code, modifier));
            }
            b'~' => match parameters[0] {
                1 | 7 => KeyCode::Home,
                2 => KeyCode::Insert,
                3 => KeyCode::Delete,
                4 | 8 => KeyCode::End,
                5 => KeyCode::PageUp,
                6 => KeyCode::PageDown,
                11..=15 => KeyCode::F((parameters[0] - 10) as u8),
                17..=21 => KeyCode::F((parameters[0] - 11) as u8),
                23..=24 => KeyCode::F((parameters[0] - 12) as u8),
                27 if parameters.len() == 3 => {
                    let Some(code) = scalar_key(parameters[2]) else {
                        return Parse::Skip(count);
                    };
                    code
                }
                _ => return Parse::Skip(count),
            },
            _ => return Parse::Skip(count),
        };
        Parse::Key(count, KeyEvent::new(code, modifier))
    }
    fn plain_key(bytes: &[u8]) -> Parse {
        let first = bytes[0];
        let (code, modifiers) = match first {
            b'\r' => (KeyCode::Enter, KeyModifiers::NONE),
            b'\t' => (KeyCode::Tab, KeyModifiers::NONE),
            0 => (KeyCode::Char(' '), KeyModifiers::CONTROL),
            1..=26 => (
                KeyCode::Char((b'a' + first - 1) as char),
                KeyModifiers::CONTROL,
            ),
            27 => (KeyCode::Esc, KeyModifiers::NONE),
            28..=31 => (
                KeyCode::Char((b'\\' + first - 28) as char),
                KeyModifiers::CONTROL,
            ),
            127 => (KeyCode::Backspace, KeyModifiers::NONE),
            32..=126 => (KeyCode::Char(first as char), KeyModifiers::NONE),
            _ => {
                let length = match first {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Parse::Skip(1),
                };
                if bytes.len() < length {
                    return Parse::Wait;
                }
                return match std::str::from_utf8(&bytes[..length])
                    .ok()
                    .and_then(|text| text.chars().next())
                {
                    Some(character) => Parse::Key(
                        length,
                        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    ),
                    None => Parse::Skip(1),
                };
            }
        };
        Parse::Key(1, KeyEvent::new(code, modifiers))
    }
}

#[cfg(windows)]
mod windows {
    use super::{Duration, io, parser::Parser};
    use crossterm::event::Event;
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use std::sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread::JoinHandle;
    use std::time::Instant;
    use windows_sys::Win32::{
        Foundation::{ERROR_OPERATION_ABORTED, HANDLE},
        Storage::FileSystem::ReadFile,
        System::{
            Console::{
                ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleCP, GetConsoleMode, GetStdHandle,
                STD_INPUT_HANDLE, SetConsoleCP, SetConsoleMode,
            },
            IO::CancelSynchronousIo,
            Threading::WaitForSingleObject,
        },
    };

    const ESCAPE_WAIT: Duration = Duration::from_millis(30);
    #[derive(Default)]
    struct Shared {
        backend: Option<Backend>,
        parser: Parser,
        escape_since: Option<Instant>,
    }
    struct Backend {
        receiver: Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
        worker: Option<JoinHandle<()>>,
        stopped: std::sync::Arc<AtomicBool>,
        input: usize,
        previous_mode: u32,
        previous_cp: u32,
    }
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    fn shared() -> &'static Mutex<Shared> {
        SHARED.get_or_init(|| Mutex::new(Shared::default()))
    }
    fn lock() -> io::Result<std::sync::MutexGuard<'static, Shared>> {
        shared()
            .lock()
            .map_err(|_| io::Error::other("terminal input lock was poisoned"))
    }
    pub struct InputGuard {
        owned: bool,
    }
    impl InputGuard {
        pub(crate) fn owns_input(&self) -> bool {
            self.owned
        }
    }
    pub fn start() -> io::Result<InputGuard> {
        let mut shared = lock()?;
        if shared.backend.is_some() {
            return Ok(InputGuard { owned: false });
        }
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let previous_cp = unsafe { GetConsoleCP() };
        if unsafe { SetConsoleMode(input, mode | ENABLE_VIRTUAL_TERMINAL_INPUT) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetConsoleCP(65001) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                SetConsoleMode(input, mode);
            }
            return Err(error);
        }
        let (sender, receiver) = mpsc::sync_channel(16);
        let stopped = std::sync::Arc::new(AtomicBool::new(false));
        let reader_stop = stopped.clone();
        let handle = input as usize;
        let worker = match std::thread::Builder::new()
            .name("aegis-terminal-input".into())
            .spawn(move || {
                while !reader_stop.load(Ordering::Acquire) {
                    let mut buffer = vec![0u8; 4096];
                    let mut count = 0;
                    let success = unsafe {
                        ReadFile(
                            handle as HANDLE,
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                            &mut count,
                            std::ptr::null_mut(),
                        )
                    };
                    if success == 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(ERROR_OPERATION_ABORTED as i32)
                            && !reader_stop.load(Ordering::Acquire)
                        {
                            let _ = sender.send(Err(error));
                        }
                        break;
                    }
                    if count == 0 {
                        break;
                    }
                    buffer.truncate(count as usize);
                    if sender.send(Ok(buffer)).is_err() {
                        break;
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                unsafe {
                    SetConsoleMode(input, mode);
                    SetConsoleCP(previous_cp);
                }
                return Err(error);
            }
        };
        shared.backend = Some(Backend {
            receiver: Some(receiver),
            worker: Some(worker),
            stopped,
            input: handle,
            previous_mode: mode,
            previous_cp,
        });
        // Full Win32 key packets retain Shift+Enter and function modifiers.
        // Standard VT remains supported for other terminals and injected input.
        if let Err(error) = io::stdout()
            .write_all(b"\x1b[?9001h")
            .and_then(|_| io::stdout().flush())
        {
            shared.backend.take();
            return Err(error);
        }
        Ok(InputGuard { owned: true })
    }
    impl Drop for Backend {
        fn drop(&mut self) {
            self.stop();
            unsafe {
                SetConsoleMode(self.input as HANDLE, self.previous_mode);
                SetConsoleCP(self.previous_cp);
            }
        }
    }
    impl Backend {
        fn stop(&mut self) -> Vec<Vec<u8>> {
            self.stopped.store(true, Ordering::Release);
            let mut buffered = Vec::new();
            if let Some(worker) = self.worker.take() {
                let handle = worker.as_raw_handle() as HANDLE;
                // Cancellation can race the next synchronous read. Repeating
                // until the worker exits closes that race without leaving a
                // cooked-mode reader behind after returning to the shell.
                loop {
                    // Drain queued typeahead while stopping. This also frees a
                    // worker blocked on its bounded channel before the join.
                    if let Some(receiver) = self.receiver.as_ref() {
                        while let Ok(chunk) = receiver.try_recv() {
                            if let Ok(bytes) = chunk {
                                buffered.push(bytes);
                            }
                        }
                    }
                    unsafe {
                        CancelSynchronousIo(handle);
                    }
                    if unsafe { WaitForSingleObject(handle, 10) } == 0 {
                        break;
                    }
                }
                let _ = worker.join();
            }
            if let Some(receiver) = self.receiver.take() {
                while let Ok(chunk) = receiver.try_recv() {
                    if let Ok(bytes) = chunk {
                        buffered.push(bytes);
                    }
                }
            }
            buffered
        }
    }
    impl Drop for InputGuard {
        fn drop(&mut self) {
            if self.owned {
                let _ = io::stdout().write_all(b"\x1b[?9001l");
                let _ = io::stdout().flush();
                if let Ok(mut shared) = lock() {
                    let chunks = shared
                        .backend
                        .as_mut()
                        .map(Backend::stop)
                        .unwrap_or_default();
                    shared.backend.take();
                    for bytes in chunks {
                        shared.parser.feed(&bytes);
                    }
                }
            }
        }
    }
    pub fn poll(timeout: Duration) -> io::Result<bool> {
        let mut shared = lock()?;
        if shared.backend.is_none() {
            return crossterm::event::poll(timeout);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if shared.parser.has_event() {
                return Ok(true);
            }
            let now = Instant::now();
            if shared.parser.escape_pending() {
                let since = *shared.escape_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= ESCAPE_WAIT {
                    shared.parser.expire_escape();
                    shared.escape_since = None;
                    return Ok(shared.parser.has_event());
                }
            } else {
                shared.escape_since = None;
            }
            let wait = deadline.saturating_duration_since(now).min(
                shared
                    .escape_since
                    .map(|since| ESCAPE_WAIT.saturating_sub(now.saturating_duration_since(since)))
                    .unwrap_or(timeout),
            );
            let result = if wait.is_zero() {
                shared
                    .backend
                    .as_ref()
                    .unwrap()
                    .receiver
                    .as_ref()
                    .unwrap()
                    .try_recv()
                    .map_err(|error| match error {
                        mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                        mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
                    })
            } else {
                shared
                    .backend
                    .as_ref()
                    .unwrap()
                    .receiver
                    .as_ref()
                    .unwrap()
                    .recv_timeout(wait)
            };
            match result {
                Ok(chunk) => shared.parser.feed(&chunk?),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input reader ended",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                    return Ok(false);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    pub fn read() -> io::Result<Event> {
        loop {
            {
                let mut shared = lock()?;
                if shared.backend.is_none() {
                    return crossterm::event::read();
                }
                if let Some(event) = shared.parser.next() {
                    return Ok(event);
                }
            }
            poll(Duration::from_secs(60))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parser::Parser;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn drain(parser: &mut Parser) -> Vec<Event> {
        std::iter::from_fn(|| parser.next()).collect()
    }
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    fn native_character(character: u16) -> Vec<u8> {
        format!("\x1b[0;0;{character};1;0;1_").into_bytes()
    }

    #[test]
    fn multiline_paste_is_one_event_even_when_utf8_and_delimiters_are_split_every_byte() {
        let text = "first\r\n  日本語 e\u{301} 👩🏽‍💻\r\nlast\x1b[31m literal";
        let bytes = format!("\x1b[200~{text}\x1b[201~");
        let mut parser = Parser::default();
        for byte in bytes.as_bytes() {
            parser.feed(&[*byte]);
        }
        assert_eq!(drain(&mut parser), vec![Event::Paste(text.into())]);
    }

    #[test]
    fn raw_input_controls_unicode_and_modified_navigation_preserve_keyboard_semantics() {
        let mut parser = Parser::default();
        let input = "日本語 e\u{301}\r\x03\x04\x0a\x1b[A\x1b[1;5D\x1bOP\x1b[20~\x1b[13;2u";
        for byte in input.as_bytes() {
            parser.feed(&[*byte]);
        }
        assert_eq!(
            drain(&mut parser),
            vec![
                key(KeyCode::Char('日'), KeyModifiers::NONE),
                key(KeyCode::Char('本'), KeyModifiers::NONE),
                key(KeyCode::Char('語'), KeyModifiers::NONE),
                key(KeyCode::Char(' '), KeyModifiers::NONE),
                key(KeyCode::Char('e'), KeyModifiers::NONE),
                key(KeyCode::Char('\u{301}'), KeyModifiers::NONE),
                key(KeyCode::Enter, KeyModifiers::NONE),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                key(KeyCode::Char('d'), KeyModifiers::CONTROL),
                key(KeyCode::Char('j'), KeyModifiers::CONTROL),
                key(KeyCode::Up, KeyModifiers::NONE),
                key(KeyCode::Left, KeyModifiers::CONTROL),
                key(KeyCode::F(1), KeyModifiers::NONE),
                key(KeyCode::F(9), KeyModifiers::NONE),
                key(KeyCode::Enter, KeyModifiers::SHIFT),
            ]
        );
    }

    #[test]
    fn win32_packets_keep_shift_enter_control_keys_repeats_and_surrogate_pairs() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b[13;28;13;1;16;1_\x1b[67;46;3;1;8;1_\x1b[112;59;0;1;0;1_\x1b[65;30;97;1;0;2_\x1b[65;30;97;0;0;1_");
        parser.feed(&native_character(0xd83d));
        parser.feed(&native_character(0xde80));
        assert_eq!(
            drain(&mut parser),
            vec![
                key(KeyCode::Enter, KeyModifiers::SHIFT),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                key(KeyCode::F(1), KeyModifiers::NONE),
                key(KeyCode::Char('a'), KeyModifiers::NONE),
                key(KeyCode::Char('a'), KeyModifiers::NONE),
                key(KeyCode::Char('🚀'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn conhost_encoded_bracketed_paste_reconstructs_unicode_and_literal_escape_sequences() {
        let text = "first\r\n日本語 e\u{301}🚀\x1b[31m";
        let mut parser = Parser::default();
        for character in format!("\x1b[200~{text}\x1b[201~").encode_utf16() {
            parser.feed(&native_character(character));
        }
        assert_eq!(drain(&mut parser), vec![Event::Paste(text.into())]);
    }

    #[test]
    fn encoded_paste_marker_escape_with_vk_escape_still_frames_one_paste() {
        let mut parser = Parser::default();
        for character in "\x1b[200~first\r\nsecond\x1b[201~".encode_utf16() {
            if character == 27 {
                parser.feed(b"\x1b[27;1;27;1;0;1_");
            } else if character == b'~' as u16 {
                parser.feed(b"\x1b[192;41;126;1;16;1_");
            } else {
                parser.feed(&native_character(character));
            }
        }
        assert_eq!(
            drain(&mut parser),
            vec![Event::Paste("first\r\nsecond".into())]
        );
    }

    #[test]
    fn classic_console_alt_numpad_text_and_native_control_j_are_preserved() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b[18;56;0;1;2;1_\x1b[99;81;0;1;2;1_\x1b[99;81;0;0;2;1_\x1b[18;56;769;0;0;1_\x1b[13;28;10;1;8;1_\x1b[65;;97;1;_");
        assert_eq!(
            drain(&mut parser),
            vec![
                key(KeyCode::Char('\u{301}'), KeyModifiers::NONE),
                key(KeyCode::Char('j'), KeyModifiers::CONTROL),
                key(KeyCode::Char('a'), KeyModifiers::NONE),
            ]
        );
    }

    #[test]
    fn paste_is_bounded_and_following_enter_remains_a_separate_submission_event() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b[200~");
        for _ in 0..100 {
            parser.feed(&[b'x'; 4096]);
        }
        parser.feed(b"\x1b[201~\r");
        let events = drain(&mut parser);
        assert!(matches!(&events[0], Event::Paste(text) if text.len() == 65_537));
        assert_eq!(events[1], key(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn repeated_native_end_marker_cannot_panic_after_paste_finishes() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b[200~data\x1b[201");
        parser.feed(b"\x1b[0;0;126;1;0;2_");
        assert_eq!(drain(&mut parser), vec![Event::Paste("data".into())]);
    }

    #[test]
    fn escape_wait_is_separate_from_partial_paste_framing_and_keeps_alt_keys() {
        let mut parser = Parser::default();
        parser.feed(b"\x1b");
        assert!(parser.escape_pending());
        parser.expire_escape();
        assert_eq!(
            drain(&mut parser),
            vec![key(KeyCode::Esc, KeyModifiers::NONE)]
        );
        parser.feed(b"\x1bx");
        assert_eq!(
            drain(&mut parser),
            vec![key(KeyCode::Char('x'), KeyModifiers::ALT)]
        );
        parser.feed(&native_character(27));
        parser.feed(&native_character(b'[' as u16));
        assert!(!parser.escape_pending());
        for byte in b"200~paste\x1b[201~" {
            parser.feed(&native_character(*byte as u16));
        }
        assert_eq!(drain(&mut parser), vec![Event::Paste("paste".into())]);
    }
}
