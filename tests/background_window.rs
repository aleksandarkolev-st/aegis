#![cfg(windows)]

use std::process::{Command, Stdio};

use anyhow::Result;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetConsoleWindow() -> *mut std::ffi::c_void;
}

#[test]
fn background_helpers_do_not_allocate_a_console_window() -> Result<()> {
    if std::env::var_os("AEGIS_TEST_BACKGROUND_CONSOLE").is_some() {
        assert!(unsafe { GetConsoleWindow() }.is_null());
        return Ok(());
    }
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "background_helpers_do_not_allocate_a_console_window",
        ])
        .env("AEGIS_TEST_BACKGROUND_CONSOLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    assert!(arun::process::spawn(command)?.wait()?.success());
    let mut direct = Command::new(std::env::current_exe()?);
    direct
        .args([
            "--exact",
            "background_helpers_do_not_allocate_a_console_window",
        ])
        .env("AEGIS_TEST_BACKGROUND_CONSOLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    assert!(arun::process::background(&mut direct).status()?.success());
    Ok(())
}
