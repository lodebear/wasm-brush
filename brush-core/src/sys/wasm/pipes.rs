use std::io::{self, Read, Write};
use std::process::Stdio;

// --- Preview1 (wasm32-wasip1) ABI -------------------------------------------------

#[cfg(target_env = "p1")]
#[link(wasm_import_module = "monocp")]
unsafe extern "C" {
    #[link_name = "pipe_create"]
    fn monocp_pipe_create(read_handle_ptr: *mut u32, write_handle_ptr: *mut u32) -> i32;
    #[link_name = "pipe_clone"]
    fn monocp_pipe_clone(handle: u32, out_handle_ptr: *mut u32) -> i32;
    #[link_name = "pipe_read"]
    fn monocp_pipe_read(handle: u32, buf_ptr: *mut u8, buf_len: u32, nread_ptr: *mut u32) -> i32;
    #[link_name = "pipe_write"]
    fn monocp_pipe_write(
        handle: u32,
        buf_ptr: *const u8,
        buf_len: u32,
        nwritten_ptr: *mut u32,
    ) -> i32;
    #[link_name = "pipe_close"]
    fn monocp_pipe_close(handle: u32) -> i32;
}

#[cfg(target_env = "p1")]
fn errno_to_error(errno: i32) -> io::Error {
    let kind = match errno {
        8 => io::ErrorKind::BrokenPipe,    // wasiErrnoBadf
        28 => io::ErrorKind::InvalidInput, // wasiErrnoInval
        29 => io::ErrorKind::Other,        // wasiErrnoIo
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("pipe error: {errno}"))
}

// --- Preview2 (wasm32-wasip2) component imports ----------------------------------

#[cfg(target_env = "p2")]
use super::monocp_p2::monocp::host::monocp;

// --- Shared pipe types ------------------------------------------------------------

/// Read end of a monocp pipe for WASM runtimes.
#[derive(Debug)]
pub struct PipeReader {
    handle: u32,
}

impl PipeReader {
    /// Clone the pipe handle.
    pub fn try_clone(&self) -> io::Result<Self> {
        let out_handle = host_pipe_clone(self.handle)?;
        Ok(Self { handle: out_handle })
    }

    /// Return the underlying handle ID.
    pub fn handle(&self) -> u32 {
        self.handle
    }
}

impl Clone for PipeReader {
    fn clone(&self) -> Self {
        self.try_clone().expect("pipe clone failed")
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        if self.handle == 0 {
            return;
        }
        let _ = host_pipe_close(self.handle);
    }
}

impl From<PipeReader> for Stdio {
    fn from(_reader: PipeReader) -> Self {
        Self::null()
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let data = host_pipe_read(self.handle, buf.len() as u32)?;
        if data.is_empty() {
            return Ok(0);
        }
        let n = std::cmp::min(buf.len(), data.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

/// Write end of a monocp pipe for WASM runtimes.
#[derive(Debug)]
pub struct PipeWriter {
    handle: u32,
}

impl PipeWriter {
    /// Clone the pipe handle.
    pub fn try_clone(&self) -> io::Result<Self> {
        let out_handle = host_pipe_clone(self.handle)?;
        Ok(Self { handle: out_handle })
    }

    /// Return the underlying handle ID.
    pub fn handle(&self) -> u32 {
        self.handle
    }
}

impl Clone for PipeWriter {
    fn clone(&self) -> Self {
        self.try_clone().expect("pipe clone failed")
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        if self.handle == 0 {
            return;
        }
        let _ = host_pipe_close(self.handle);
    }
}

impl From<PipeWriter> for Stdio {
    fn from(_writer: PipeWriter) -> Self {
        Self::null()
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let nwritten = host_pipe_write(self.handle, buf)?;
        Ok(nwritten as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Create a new pipe backed by the monocp host.
pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let (read_handle, write_handle) = host_pipe_create()?;
    Ok((
        PipeReader {
            handle: read_handle,
        },
        PipeWriter {
            handle: write_handle,
        },
    ))
}

// --- Host call shims --------------------------------------------------------------

fn host_pipe_create() -> io::Result<(u32, u32)> {
    #[cfg(target_env = "p1")]
    {
        let mut read_handle = 0_u32;
        let mut write_handle = 0_u32;
        let rc = unsafe { monocp_pipe_create(&mut read_handle, &mut write_handle) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        return Ok((read_handle, write_handle));
    }

    #[cfg(target_env = "p2")]
    {
        let handles = monocp::pipe_create();
        return Ok((handles.read, handles.write));
    }

    #[allow(unreachable_code)]
    Err(io::Error::other("unsupported target"))
}

fn host_pipe_clone(handle: u32) -> io::Result<u32> {
    #[cfg(target_env = "p1")]
    {
        let mut out_handle = 0_u32;
        let rc = unsafe { monocp_pipe_clone(handle, &mut out_handle) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        return Ok(out_handle);
    }

    #[cfg(target_env = "p2")]
    {
        return Ok(monocp::pipe_clone(handle));
    }

    #[allow(unreachable_code)]
    Err(io::Error::other("unsupported target"))
}

fn host_pipe_read(handle: u32, max_len: u32) -> io::Result<Vec<u8>> {
    #[cfg(target_env = "p1")]
    {
        if max_len == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0_u8; max_len as usize];
        let mut nread = 0_u32;
        let rc = unsafe { monocp_pipe_read(handle, buf.as_mut_ptr(), max_len, &mut nread) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        buf.truncate(nread as usize);
        return Ok(buf);
    }

    #[cfg(target_env = "p2")]
    {
        return Ok(monocp::pipe_read(handle, max_len));
    }

    #[allow(unreachable_code)]
    Err(io::Error::other("unsupported target"))
}

fn host_pipe_write(handle: u32, data: &[u8]) -> io::Result<u32> {
    #[cfg(target_env = "p1")]
    {
        let mut nwritten = 0_u32;
        let rc =
            unsafe { monocp_pipe_write(handle, data.as_ptr(), data.len() as u32, &mut nwritten) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        return Ok(nwritten);
    }

    #[cfg(target_env = "p2")]
    {
        return Ok(monocp::pipe_write(handle, data));
    }

    #[allow(unreachable_code)]
    Err(io::Error::other("unsupported target"))
}

fn host_pipe_close(handle: u32) -> io::Result<()> {
    #[cfg(target_env = "p1")]
    {
        let rc = unsafe { monocp_pipe_close(handle) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        return Ok(());
    }

    #[cfg(target_env = "p2")]
    {
        monocp::pipe_close(handle);
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(io::Error::other("unsupported target"))
}
