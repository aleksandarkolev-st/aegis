use std::io;
use std::process::{Command, ExitStatus};

use process_wrap::std::{ChildWrapper, CommandWrap};

pub struct Child {
    inner: Box<dyn ChildWrapper>,
    #[cfg(windows)]
    _lifetime_job: std::os::windows::io::OwnedHandle,
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
    #[cfg(windows)]
    let lifetime_job = lifetime_job()?;
    let mut command = CommandWrap::from(command);
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    let inner = command.spawn()?;
    #[cfg(windows)]
    let mut inner = inner;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let assigned = inner.process_handle().is_some_and(|handle| unsafe {
            AssignProcessToJobObject(lifetime_job.as_raw_handle(), handle.as_raw_handle()) != 0
        });
        if !assigned {
            let error = io::Error::last_os_error();
            let _ = inner.start_kill();
            return Err(error);
        }
    }
    Ok(Child {
        inner,
        #[cfg(windows)]
        _lifetime_job: lifetime_job,
    })
}

#[cfg(windows)]
fn lifetime_job() -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}
