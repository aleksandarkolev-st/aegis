use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::terminal::{RawMode, Terminal};
use crate::terminal_input as event;

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

pub fn run<Value: Send + 'static>(
    terminal: &Terminal,
    label: &str,
    operation: impl FnOnce(Arc<AtomicBool>) -> Result<Value> + Send + 'static,
) -> Result<Option<Value>> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("aegis-discovery".into())
        .spawn(move || {
            let result = operation(worker_cancelled);
            let _ = sender.send(result);
        })
        .map_err(|_| anyhow!("Could not start Aegis's discovery worker"))?;
    let _worker = Worker {
        cancelled: cancelled.clone(),
        thread: Some(thread),
    };
    present(terminal, label, &receiver, &cancelled)
}

fn present<Value>(
    terminal: &Terminal,
    label: &str,
    receiver: &mpsc::Receiver<Result<Value>>,
    cancelled: &AtomicBool,
) -> Result<Option<Value>> {
    let _raw = RawMode::enter(terminal.interactive)?;
    let started = Instant::now();
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                terminal.clear_activity()?;
                if cancelled.load(Ordering::Acquire) {
                    return Ok(None);
                }
                return result.map(Some);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminal.clear_activity()?;
                bail!("Aegis's discovery worker stopped before returning metadata");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if terminal.interactive && event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if cancel_key(key) {
                    cancelled.store(true, Ordering::Release);
                }
            }
        }
        terminal.loading_activity(
            if cancelled.load(Ordering::Acquire) {
                "Cancelling model discovery"
            } else {
                label
            },
            started.elapsed(),
            "Esc / Ctrl+C cancel · model selection stays unchanged",
        )?;
    }
}

pub(crate) fn cancel_key(key: crossterm::event::KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && (key.code == KeyCode::Esc
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_returns_values_and_failures_without_reporting_false_success() -> Result<()> {
        let terminal = Terminal::default();
        assert_eq!(run(&terminal, "Fixture discovery", |_| Ok(7))?, Some(7));
        assert!(
            run::<()>(&terminal, "Fixture discovery", |_| bail!("Fixture failure"))
                .unwrap_err()
                .to_string()
                .contains("Fixture failure")
        );
        let (sender, receiver) = mpsc::channel();
        sender.send(Ok(7))?;
        assert_eq!(
            present(
                &terminal,
                "Fixture discovery",
                &receiver,
                &AtomicBool::new(true)
            )?,
            None
        );
        let (sender, receiver) = mpsc::channel::<Result<()>>();
        drop(sender);
        assert!(
            present(
                &terminal,
                "Fixture discovery",
                &receiver,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn worker_teardown_cancels_and_joins_owned_work() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker_stopped = stopped.clone();
        let worker = Worker {
            cancelled: cancelled.clone(),
            thread: Some(std::thread::spawn(move || {
                while !worker_cancelled.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                worker_stopped.store(true, Ordering::Release);
            })),
        };
        drop(worker);
        assert!(cancelled.load(Ordering::Acquire));
        assert!(stopped.load(Ordering::Acquire));
    }
}
