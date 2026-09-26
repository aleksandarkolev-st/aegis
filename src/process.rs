use std::io;
use std::process::{Command, ExitStatus};

use process_wrap::std::{ChildWrapper, CommandWrap};

pub struct Child {
    inner: Box<dyn ChildWrapper>,
}

impl Child {
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.inner.start_kill()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.inner.start_kill();
    }
}

pub fn spawn(command: Command) -> io::Result<Child> {
    let mut command = CommandWrap::from(command);
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    Ok(Child {
        inner: command.spawn()?,
    })
}
