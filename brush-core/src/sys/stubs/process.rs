//! Process management utilities

pub(crate) type ProcessId = i32;

/// Provides access to a child process.
pub struct Child {
    inner: std::process::Child,
}

pub(crate) use std::process::ExitStatus;
pub(crate) use std::process::Output;

impl Child {
    /// Returns the process ID of the child process, if available.
    pub fn id(&self) -> Option<u32> {
        None
    }

    /// Asynchronously waits for the child process to exit.
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.inner.wait()
    }

    /// Asynchronously waits for the child process to exit and collects its
    /// output.
    pub async fn wait_with_output(self) -> std::io::Result<Output> {
        self.inner.wait_with_output()
    }
}

pub(crate) fn spawn(mut command: std::process::Command) -> std::io::Result<Child> {
    let child = command.spawn()?;
    Ok(Child { inner: child })
}

#[cfg(all(target_arch = "wasm32", target_env = "p2"))]
pub(crate) fn current_process_id() -> u32 {
    crate::sys::wasm::monocp_p2::monocp::host::monocp::process_id()
}

#[cfg(not(all(target_arch = "wasm32", target_env = "p2")))]
pub(crate) fn current_process_id() -> u32 {
    std::process::id()
}
