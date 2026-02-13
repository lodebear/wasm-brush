pub use crate::sys::stubs::commands;
pub use crate::sys::stubs::fd;
pub use crate::sys::stubs::fs;
pub use crate::sys::stubs::input;
pub(crate) use crate::sys::stubs::network;

#[cfg(target_env = "p2")]
pub(crate) mod monocp_p2;

/// Pipe support for monocp host integrations.
pub mod pipes;
pub use crate::sys::stubs::poll;
pub use crate::sys::stubs::process;
pub use crate::sys::stubs::resource;
pub use crate::sys::stubs::signal;
pub use crate::sys::stubs::terminal;
pub(crate) use crate::sys::stubs::users;

/// Platform-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {}
