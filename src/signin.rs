use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::Event;

use crate::auth_store::Vault;
use crate::oauth::{AuthClient, BrowserLogin, DeviceLogin, Poll};
use crate::terminal::{RawMode, Terminal, Tone};
use crate::terminal_input as event;

enum Update {
    Browser {
        url: String,
        seconds: u64,
    },
    Code {
        url: String,
        code: String,
        seconds: u64,
    },
    Done(Result<()>),
}

enum Login {
    Browser(BrowserLogin),
    Device(DeviceLogin),
}

struct Worker {
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn provider_name(provider: crate::direct::Provider) -> &'static str {
    match provider {
        crate::direct::Provider::ChatGpt => "chatgpt",
        crate::direct::Provider::Grok => "grok",
    }
}

pub fn ensure(provider: &str, terminal: &Terminal) -> Result<bool> {
    let selected = match crate::direct::provider(provider) {
        Ok(provider) => provider,
        Err(error) => {
            terminal.message(Tone::Warning, "Provider", &error.to_string())?;
            return Ok(false);
        }
    };
    let vault = Vault::user()?;
    match vault.load(provider_name(selected)) {
        Ok(Some(session)) if session.credentials().is_ok() || session.refresh_token.is_some() => {
            return Ok(true);
        }
        Ok(_) => {}
        Err(error) => terminal.message(Tone::Warning, "Saved sign-in", &error.to_string())?,
    }
    run(provider, terminal)
}

pub fn run(provider: &str, terminal: &Terminal) -> Result<bool> {
    run_with_status(provider, terminal, false)
}

pub fn command(provider: &str, terminal: &Terminal) -> Result<()> {
    run_with_status(provider, terminal, true).map(|_| ())
}

fn run_with_status(provider: &str, terminal: &Terminal, propagate_failure: bool) -> Result<bool> {
    let selected = crate::direct::provider(provider)?;
    let vault = Vault::user()?;
    let existing = vault.load(provider_name(selected))?.is_some();
    let mut choices = vec![
        "Sign in in your browser · recommended".to_owned(),
        "Use a device code · remote or headless terminal".to_owned(),
    ];
    if existing {
        choices.push("Remove Aegis's saved sign-in".into());
    }
    choices.push("Back".into());
    let Some(choice) = terminal.select("Aegis sign-in · no provider CLI required", &choices)?
    else {
        return Ok(false);
    };
    if existing && choice == 2 {
        AuthClient::new(selected)?.logout(&vault, || false)?;
        terminal.message(
            Tone::Success,
            "Signed out",
            "Only Aegis's saved sign-in was removed.",
        )?;
        return Ok(false);
    }
    if choice > 1 {
        return Ok(false);
    }
    if cfg!(not(windows)) {
        terminal.message(Tone::Quiet, "Storage", "Aegis stores this session in owner-only files. This platform's file storage is not encrypted.")?;
    }
    terminal.message(Tone::Quiet, "Sign in", "Approve only the sign-in you requested here. Your chats, tasks and native provider credentials are not part of this sign-in.")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    let worker_cancelled = cancelled.clone();
    let thread = std::thread::spawn(move || {
        let interrupted = || worker_cancelled.load(Ordering::Acquire);
        let outcome = (|| -> Result<()> {
            let client = AuthClient::new(selected)?;
            let mut login = if choice == 0 {
                let login = client.begin_browser(interrupted)?;
                sender
                    .send(Update::Browser {
                        url: login.authorization_url().to_owned(),
                        seconds: login.expires_in().as_secs(),
                    })
                    .map_err(|_| anyhow!("Sign-in interface closed"))?;
                Login::Browser(login)
            } else {
                let login = client.begin(interrupted)?;
                sender
                    .send(Update::Code {
                        url: login.verification_url().to_owned(),
                        code: login.user_code().to_owned(),
                        seconds: login.expires_in().as_secs(),
                    })
                    .map_err(|_| anyhow!("Sign-in interface closed"))?;
                Login::Device(login)
            };
            loop {
                let outcome = match &mut login {
                    Login::Browser(login) => client.poll_browser(login, interrupted)?,
                    Login::Device(login) => client.poll(login, interrupted)?,
                };
                match outcome {
                    Poll::SignedIn(session) => return client.save(&vault, &session, interrupted),
                    Poll::Pending(delay) => {
                        let wait = Instant::now() + delay;
                        while Instant::now() < wait {
                            if interrupted() {
                                bail!("Sign-in cancelled");
                            }
                            std::thread::sleep(
                                wait.saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(25)),
                            );
                        }
                    }
                }
            }
        })();
        let _ = sender.send(Update::Done(outcome));
    });
    let _worker = Worker {
        cancelled,
        thread: Some(thread),
    };
    report_outcome(
        terminal,
        present(terminal, &receiver, &_worker.cancelled),
        propagate_failure,
    )
}

fn report_outcome(
    terminal: &Terminal,
    result: Result<bool>,
    propagate_failure: bool,
) -> Result<bool> {
    match result {
        Err(error) if !propagate_failure => {
            terminal.message(Tone::Warning, "Sign-in", &error.to_string())?;
            Ok(false)
        }
        outcome => outcome,
    }
}

fn present(
    terminal: &Terminal,
    receiver: &mpsc::Receiver<Update>,
    cancelled: &AtomicBool,
) -> Result<bool> {
    let _raw = RawMode::enter(terminal.interactive)?;
    let started = Instant::now();
    let mut label = "Connecting securely".to_owned();
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Update::Browser { url, seconds }) => {
                terminal.clear_activity()?;
                terminal.message(
                    Tone::Accent,
                    "Browser sign-in",
                    &format!(
                        "Approve within {} minutes; Aegis will continue in this terminal.",
                        seconds.div_ceil(60)
                    ),
                )?;
                terminal.message(Tone::Quiet, "Privacy", "This link belongs to this sign-in attempt. Never share it or the returned callback.")?;
                if !terminal.interactive || !open_browser(&url) {
                    terminal.message(Tone::Accent, "Open", &url)?;
                    terminal.message(Tone::Quiet, "Browser", "Open this link in your browser. If you are on a remote terminal, cancel and choose device-code sign-in.")?;
                }
                label = "Waiting for your browser approval".into();
            }
            Ok(Update::Code { url, code, seconds }) => {
                terminal.clear_activity()?;
                terminal.message(Tone::Accent, "Open", &url)?;
                terminal.message(
                    Tone::Accent,
                    "Your code",
                    &format!("{code} · approve within {} minutes", seconds.div_ceil(60)),
                )?;
                terminal.message(
                    Tone::Quiet,
                    "Privacy",
                    "This code authorizes your account. Never share it with anyone.",
                )?;
                if terminal.interactive && !open_browser(&url) {
                    terminal.message(
                        Tone::Quiet,
                        "Browser",
                        "Open the link above; Aegis will continue automatically after approval.",
                    )?;
                }
                label = "Waiting for your browser approval".into();
            }
            Ok(Update::Done(result)) => {
                terminal.clear_activity()?;
                if cancelled.load(Ordering::Acquire) {
                    terminal.message(
                        Tone::Quiet,
                        "Cancelled",
                        "No task was started. F4 lets you check or remove Aegis's sign-in.",
                    )?;
                    return Ok(false);
                }
                match result {
                    Ok(()) => {
                        terminal.message(
                            Tone::Success,
                            "Signed in",
                            "Aegis is connected directly. No native agent CLI is required.",
                        )?;
                        return Ok(true);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminal.clear_activity()?;
                bail!("Sign-in worker stopped; no task was started");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        terminal.authentication_activity(&label, started.elapsed())?;
        if terminal.interactive && event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if cancel_key(key) {
                    cancelled.store(true, Ordering::Release);
                    label = "Cancelling sign-in".into();
                }
            }
        }
    }
}

fn cancel_key(key: crossterm::event::KeyEvent) -> bool {
    crate::background::cancel_key(key)
}

#[cfg(windows)]
fn open_browser(url: &str) -> bool {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
    };
    let initialized = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    if initialized < 0 {
        return false;
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    let verb = "open\0".encode_utf16().collect::<Vec<_>>();
    let target = url.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    result as isize > 32
}

#[cfg(not(windows))]
fn open_browser(_url: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn sign_in_cancellation_is_explicit_and_never_consumes_model_commands() {
        assert!(cancel_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(cancel_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(cancel_key(KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL
        )));
        assert!(!cancel_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        let mut release = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!cancel_key(release));
    }

    #[test]
    fn failed_worker_and_cancelled_completion_never_report_a_successful_sign_in() -> Result<()> {
        let terminal = Terminal::default();
        if terminal.interactive {
            return Ok(());
        }
        let (sender, receiver) = mpsc::channel();
        sender.send(Update::Done(Err(anyhow!("Fixture connection failed"))))?;
        let failed = present(&terminal, &receiver, &AtomicBool::new(false));
        assert!(failed.is_err());
        assert!(report_outcome(&terminal, failed, true).is_err());
        assert!(!report_outcome(
            &terminal,
            Err(anyhow!("Fixture connection failed")),
            false
        )?);
        assert!(!report_outcome(&terminal, Ok(false), true)?);
        sender.send(Update::Done(Ok(())))?;
        assert!(!present(&terminal, &receiver, &AtomicBool::new(true))?);
        drop(sender);
        assert!(present(&terminal, &receiver, &AtomicBool::new(false)).is_err());
        Ok(())
    }
}
