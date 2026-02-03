use std::io::{self, Read, Write};
use std::process::Stdio;

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

fn errno_to_error(errno: i32) -> io::Error {
    let kind = match errno {
        8 => io::ErrorKind::BrokenPipe,    // wasiErrnoBadf
        28 => io::ErrorKind::InvalidInput, // wasiErrnoInval
        29 => io::ErrorKind::Other,        // wasiErrnoIo
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("pipe error: {errno}"))
}

/// Read end of a monocp pipe for WASM runtimes.
#[derive(Debug)]
pub struct PipeReader {
    handle: u32,
}

impl PipeReader {
    /// Clone the pipe handle.
    pub fn try_clone(&self) -> io::Result<Self> {
        let mut out_handle = 0_u32;
        let rc = unsafe { monocp_pipe_clone(self.handle, &mut out_handle) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
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
        unsafe {
            let _ = monocp_pipe_close(self.handle);
        }
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
        let mut nread = 0_u32;
        let rc = unsafe {
            monocp_pipe_read(self.handle, buf.as_mut_ptr(), buf.len() as u32, &mut nread)
        };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        Ok(nread as usize)
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
        let mut out_handle = 0_u32;
        let rc = unsafe { monocp_pipe_clone(self.handle, &mut out_handle) };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
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
        unsafe {
            let _ = monocp_pipe_close(self.handle);
        }
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
        let mut nwritten = 0_u32;
        let rc = unsafe {
            monocp_pipe_write(self.handle, buf.as_ptr(), buf.len() as u32, &mut nwritten)
        };
        if rc != 0 {
            return Err(errno_to_error(rc));
        }
        Ok(nwritten as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Create a new pipe backed by the monocp host.
pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let mut read_handle = 0_u32;
    let mut write_handle = 0_u32;
    let rc = unsafe { monocp_pipe_create(&mut read_handle, &mut write_handle) };
    if rc != 0 {
        return Err(errno_to_error(rc));
    }
    Ok((
        PipeReader {
            handle: read_handle,
        },
        PipeWriter {
            handle: write_handle,
        },
    ))
}
