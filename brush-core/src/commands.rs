//! Command execution

use std::{
    borrow::Cow,
    ffi::OsStr,
    fmt::Display,
    path::{Path, PathBuf},
    process::Stdio,
};

use brush_parser::ast;
use itertools::Itertools;
#[cfg(not(target_arch = "wasm32"))]
use sys::commands::CommandFgControlExt;
use sys::commands::{CommandExt, CommandFdInjectionExt};

#[cfg(not(target_arch = "wasm32"))]
use crate::{ErrorKind, pathsearch, trace_categories};
use crate::{
    ExecutionControlFlow, ExecutionExitCode, ExecutionParameters, ExecutionResult, Shell, ShellFd,
    builtins, commands, env, error, escape,
    extensions::{self, ShellExtensions},
    functions,
    interp::{self, Execute, ProcessGroupPolicy},
    openfiles::{self, OpenFile, OpenFiles},
    processes,
    results::ExecutionSpawnResult,
    sys, traps, variables,
};

/// Encapsulates the result of waiting for a command to complete.
pub enum CommandWaitResult {
    /// The command completed.
    CommandCompleted(ExecutionResult),
    /// The command was stopped before it completed.
    CommandStopped(ExecutionResult, processes::ChildProcess),
}

/// Represents the context for executing a command.
pub struct ExecutionContext<'a, SE: ShellExtensions = extensions::DefaultShellExtensions> {
    /// The shell in which the command is being executed.
    pub shell: &'a mut Shell<SE>,
    /// The name of the command being executed.    
    pub command_name: String,
    /// The parameters for the execution.
    pub params: ExecutionParameters,
}

impl<SE: ShellExtensions> ExecutionContext<'_, SE> {
    /// Returns the standard input file; usable with `write!` et al.
    pub fn stdin(&self) -> impl std::io::Read + 'static {
        self.params.stdin(self.shell)
    }

    /// Returns the standard output file; usable with `write!` et al.
    pub fn stdout(&self) -> impl std::io::Write + 'static {
        self.params.stdout(self.shell)
    }

    /// Returns the standard error file; usable with `write!` et al.
    pub fn stderr(&self) -> impl std::io::Write + 'static {
        self.params.stderr(self.shell)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(&self, fd: ShellFd) -> Option<openfiles::OpenFile> {
        self.params.try_fd(self.shell, fd)
    }

    /// Iterates over all open file descriptors.
    pub fn iter_fds(&self) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        self.params.iter_fds(self.shell)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn should_cmd_lead_own_process_group(&self) -> bool {
        self.shell.options().interactive
            && matches!(
                self.params.process_group_policy,
                ProcessGroupPolicy::NewProcessGroup
            )
    }
}

/// An argument to a command.
#[derive(Clone, Debug)]
pub enum CommandArg {
    /// A simple string argument.
    String(String),
    /// An assignment/declaration; typically treated as a string, but will
    /// be specially handled by a limited set of built-in commands.
    Assignment(ast::Assignment),
}

impl Display for CommandArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Assignment(a) => write!(f, "{a}"),
        }
    }
}

impl From<String> for CommandArg {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&String> for CommandArg {
    fn from(value: &String) -> Self {
        Self::String(value.clone())
    }
}

impl CommandArg {
    pub(crate) fn quote_for_tracing(&self) -> Cow<'_, str> {
        match self {
            Self::String(s) => escape::quote_if_needed(s, escape::QuoteMode::SingleQuote),
            Self::Assignment(a) => {
                let mut s = a.name.to_string();
                let op = if a.append { "+=" } else { "=" };
                s.push_str(op);
                s.push_str(&escape::quote_if_needed(
                    a.value.to_string().as_str(),
                    escape::QuoteMode::SingleQuote,
                ));
                s.into()
            }
        }
    }
}

/// Encapsulates a possibly-owned reference to a `Shell` for command execution.
pub enum ShellForCommand<'a, SE: extensions::ShellExtensions> {
    /// The command is run in the same shell as its parent; the provided
    /// mutable reference allows modifying the parent shell.
    ParentShell(&'a mut Shell<SE>),
    /// The command is run in its own owned shell (which is also provided).
    OwnedShell {
        /// The owned shell.
        target: Box<Shell<SE>>,
        /// The parent shell.
        parent: &'a mut Shell<SE>,
    },
}

impl<SE: extensions::ShellExtensions> std::ops::Deref for ShellForCommand<'_, SE> {
    type Target = Shell<SE>;

    fn deref(&self) -> &Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

impl<SE: extensions::ShellExtensions> std::ops::DerefMut for ShellForCommand<'_, SE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            ShellForCommand::ParentShell(shell) => shell,
            ShellForCommand::OwnedShell { target, .. } => target,
        }
    }
}

/// Composes a `std::process::Command` to execute the given command. Appropriately
/// configures the command name and arguments, redirections, injected file
/// descriptors, environment variables, etc.
///
/// # Arguments
///
/// * `context` - The execution context in which the command is being composed.
/// * `command_name` - The name of the command to execute.
/// * `argv0` - The value to use for `argv[0]` (may be different from the command).
/// * `args` - The arguments to pass to the command.
/// * `empty_env` - If true, the command will be executed with an empty environment; if false, the
///   command will inherit environment variables marked as exported in the provided `Shell`.
#[allow(unused_variables, reason = "argv0 is only used on unix platforms")]
pub fn compose_std_command<S: AsRef<OsStr>, SE: extensions::ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    command_name: &str,
    argv0: &str,
    args: &[S],
    empty_env: bool,
) -> Result<std::process::Command, error::Error> {
    let mut cmd = std::process::Command::new(command_name);

    // Override argv[0].
    // NOTE: Not supported on all platforms.
    cmd.arg0(argv0);

    // Pass through args.
    cmd.args(args);

    // Use the shell's current working dir.
    cmd.current_dir(context.shell.working_dir());

    // Start with a clear environment.
    cmd.env_clear();

    // Add in exported variables.
    if !empty_env {
        for (k, v) in context.shell.env().iter_exported() {
            // NOTE: To match bash behavior, we only include exported variables
            // that are set (i.e., have a value). This means a variable that
            // shows up in `declare -p` but has no *set* value will be omitted.
            if v.value().is_set() {
                cmd.env(k.as_str(), v.value().to_cow_str(context.shell).as_ref());
            }
        }
    }

    // Add in exported functions.
    if !empty_env {
        for (func_name, registration) in context.shell.funcs().iter() {
            if registration.is_exported() {
                let var_name = std::format!("BASH_FUNC_{func_name}%%");
                let value = std::format!("() {}", registration.definition().body);
                cmd.env(var_name, value);
            }
        }
    }

    // Redirect stdin, if applicable.
    match context.try_fd(OpenFiles::STDIN_FD) {
        Some(OpenFile::Stdin(_)) | None => (),
        Some(stdin_file) => {
            let as_stdio: Stdio = stdin_file.into();
            cmd.stdin(as_stdio);
        }
    }

    // Redirect stdout, if applicable.
    match context.try_fd(OpenFiles::STDOUT_FD) {
        Some(OpenFile::Stdout(_)) | None => (),
        Some(stdout_file) => {
            let as_stdio: Stdio = stdout_file.into();
            cmd.stdout(as_stdio);
        }
    }

    // Redirect stderr, if applicable.
    match context.try_fd(OpenFiles::STDERR_FD) {
        Some(OpenFile::Stderr(_)) | None => {}
        Some(stderr_file) => {
            let as_stdio: Stdio = stderr_file.into();
            cmd.stderr(as_stdio);
        }
    }

    // Inject any other fds.
    let other_files = context.iter_fds().filter(|(fd, _)| {
        *fd != OpenFiles::STDIN_FD && *fd != OpenFiles::STDOUT_FD && *fd != OpenFiles::STDERR_FD
    });
    cmd.inject_fds(other_files)?;

    Ok(cmd)
}

pub(crate) async fn on_preexecute(
    cmd: &mut commands::SimpleCommand<'_, impl extensions::ShellExtensions>,
) -> Result<(), error::Error> {
    // See if we have a DEBUG trap handler registered; call it if we do.
    invoke_debug_trap_handler_if_registered(&mut cmd.shell, &cmd.params, cmd.args.as_slice())
        .await?;

    Ok(())
}

async fn invoke_debug_trap_handler_if_registered(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    args: &[CommandArg],
) -> Result<(), error::Error> {
    if shell.call_stack().trap_handler_depth() > 0 {
        return Ok(());
    }

    let Some(debug_trap_handler) = shell.traps().get_handler(traps::TrapSignal::Debug).cloned()
    else {
        return Ok(());
    };

    // TODO(traps): Confirm whether trap handlers should be executed in the same process
    // group.
    let mut handler_params = params.clone();
    handler_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    let full_cmd = args.iter().map(|arg| arg.to_string()).join(" ");

    // TODO(well-known-vars): This shouldn't *just* be set in a trap situation.
    shell.env_mut().update_or_add(
        "BASH_COMMAND",
        variables::ShellValueLiteral::Scalar(full_cmd),
        |_| Ok(()),
        env::EnvironmentLookup::Anywhere,
        env::EnvironmentScope::Global,
    )?;

    shell.enter_trap_handler(Some(&debug_trap_handler));

    // TODO(traps): Discard result?
    let _ = shell
        .run_string(
            &debug_trap_handler.command,
            &debug_trap_handler.source_info,
            &handler_params,
        )
        .await;

    shell.leave_trap_handler();

    Ok(())
}

/// Represents a simple command to be executed.
pub struct SimpleCommand<'a, SE: extensions::ShellExtensions> {
    /// The shell to run the command in.
    shell: ShellForCommand<'a, SE>,

    /// The execution parameters for the command.
    pub params: ExecutionParameters,

    /// The name of the command to execute.
    pub command_name: String,

    /// The arguments to the command, including the command itself.
    pub args: Vec<CommandArg>,

    /// Whether to consider shell functions when looking up the command name.
    /// If true, shell functions will be checked; if false, they will be ignored.
    pub use_functions: bool,

    /// Optional list of directories to search for external commands. If left
    /// `None`, the default search logic will be used.
    pub path_dirs: Option<Vec<PathBuf>>,

    /// The process group ID to use for externally executed commands. This may be
    /// `None`, in which case the default behavior will be used.
    pub process_group_id: Option<i32>,

    /// Optionally provides a function that can run after execution occurs. Note
    /// that it is *not* invoked if the shell is discarded during the execution
    /// process.
    #[allow(clippy::type_complexity)]
    pub post_execute: Option<fn(&mut Shell<SE>) -> Result<(), error::Error>>,
}

impl<'a, SE: extensions::ShellExtensions> SimpleCommand<'a, SE> {
    /// Creates a new `SimpleCommand` instance.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell in which to execute the command.
    /// * `params` - The execution parameters for the command.
    /// * `command_name` - The name of the command to execute.
    /// * `args` - The arguments to the command, including the command itself.
    pub const fn new(
        shell: ShellForCommand<'a, SE>,
        params: ExecutionParameters,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> Self {
        Self {
            shell,
            params,
            command_name,
            args,
            use_functions: true,
            path_dirs: None,
            process_group_id: None,
            post_execute: None,
        }
    }

    /// Executes the simple command.
    ///
    /// The command may be a builtin, a shell function, or an externally
    /// executed command. This function's implementation is responsible for
    /// dispatching it appropriately according to the context provided.
    #[allow(
        clippy::missing_panics_doc,
        reason = "these unwrap calls should not panic"
    )]
    #[cfg_attr(target_arch = "wasm32", allow(unused_mut))]
    pub async fn execute(mut self) -> Result<ExecutionSpawnResult, error::Error> {
        // First see if it's the name of a builtin.
        let builtin = self.shell.builtins().get(&self.command_name).cloned();

        // If we're in POSIX mode and found a special builtin (that's not disabled), then invoke it
        // without considering functions.
        if self.shell.options().posix_mode
            && builtin
                .as_ref()
                .is_some_and(|r| !r.disabled && r.special_builtin)
        {
            #[allow(clippy::unwrap_used, reason = "we just checked that builtin is Some")]
            let builtin = builtin.unwrap();
            return self.execute_via_builtin(builtin).await;
        }

        // Assuming we weren't requested not to do so, check if it's the name of
        // a shell function.
        if self.use_functions {
            if let Some(func_registration) =
                self.shell.funcs().get(self.command_name.as_str()).cloned()
            {
                return self.execute_via_function(func_registration).await;
            }
        }

        // If we haven't yet resolved the command name and found a builtin that's not disabled,
        // then invoke it.
        if let Some(builtin) = builtin {
            if !builtin.disabled {
                return self.execute_via_builtin(builtin).await;
            }
        }

        // We still haven't found a command to invoke. We'll need to look for an external command.
        if !self.command_name.contains(std::path::MAIN_SEPARATOR) {
            #[cfg(target_arch = "wasm32")]
            {
                let command_name = self.command_name.clone();
                return self.execute_via_external(Path::new(command_name.as_str()));
            }

            #[cfg(not(target_arch = "wasm32"))]
            {
                // All else failed; if we were given path directories to search, try to look through
                // them for a matching executable. Otherwise, use our default search logic.
                let path = if let Some(path_dirs) = &self.path_dirs {
                    pathsearch::search_for_executable(path_dirs.iter(), self.command_name.as_str())
                        .next()
                } else {
                    self.shell
                        .find_first_executable_in_path_using_cache(&self.command_name)
                };

                if let Some(path) = path {
                    self.execute_via_external(&path)
                } else {
                    if let Some(post_execute) = self.post_execute {
                        let _ = post_execute(&mut self.shell);
                    }

                    Err(ErrorKind::CommandNotFound(self.command_name).into())
                }
            }
        } else {
            let command_name = PathBuf::from(self.command_name.clone());
            self.execute_via_external(command_name.as_path())
        }
    }

    async fn execute_via_builtin(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        match self.shell {
            ShellForCommand::OwnedShell { target, .. } => {
                #[cfg(target_arch = "wasm32")]
                {
                    let mut shell = *target;
                    let cmd_context = ExecutionContext {
                        shell: &mut shell,
                        command_name: self.command_name,
                        params: self.params,
                    };

                    let result = execute_builtin_command(&builtin, cmd_context, self.args).await;

                    if let Some(post_execute) = self.post_execute {
                        let _ = post_execute(&mut shell);
                    }

                    let result = result?;
                    return Ok(result.into());
                }

                #[cfg(not(target_arch = "wasm32"))]
                {
                    Ok(Self::execute_via_builtin_in_owned_shell(
                        *target,
                        self.params,
                        builtin,
                        self.command_name,
                        self.args,
                    ))
                }
            }
            ShellForCommand::ParentShell(..) => {
                self.execute_via_builtin_in_parent_shell(builtin).await
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn execute_via_builtin_in_owned_shell(
        mut shell: Shell<SE>,
        params: ExecutionParameters,
        builtin: builtins::Registration<SE>,
        command_name: String,
        args: Vec<CommandArg>,
    ) -> ExecutionSpawnResult {
        let join_handle = tokio::task::spawn_blocking(move || {
            let cmd_context = ExecutionContext {
                shell: &mut shell,
                command_name,
                params,
            };

            let rt = tokio::runtime::Handle::current();
            rt.block_on(execute_builtin_command(&builtin, cmd_context, args))
        });

        ExecutionSpawnResult::StartedTask(join_handle)
    }

    async fn execute_via_builtin_in_parent_shell(
        self,
        builtin: builtins::Registration<SE>,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        let result = execute_builtin_command(&builtin, cmd_context, self.args).await;

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        let result = result?;

        Ok(result.into())
    }

    async fn execute_via_function(
        self,
        func_registration: functions::Registration,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        // Strip the function name off args.
        let result = invoke_shell_function(func_registration, cmd_context, &self.args[1..]).await;

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        result
    }

    fn execute_via_external(self, path: &Path) -> Result<ExecutionSpawnResult, error::Error> {
        let mut shell = self.shell;

        let cmd_context = ExecutionContext {
            shell: &mut shell,
            command_name: self.command_name,
            params: self.params,
        };

        let resolved_path = path.to_string_lossy();
        let result = execute_external_command(
            cmd_context,
            resolved_path.as_ref(),
            self.process_group_id,
            &self.args[1..],
        );

        if let Some(post_execute) = self.post_execute {
            let _ = post_execute(&mut shell);
        }

        result
    }
}

#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
#[repr(C)]
struct MonocpExecContextV2 {
    argv_ptr: u32,
    argv_len: u32,
    cwd_ptr: u32,
    cwd_len: u32,
    fds_ptr: u32,
    fds_len: u32,
}

#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
#[repr(C)]
struct MonocpFdSpec {
    child_fd: u32,
    kind: u32,
    value: u32,
    flags: u32,
}

#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
const MONOCP_FD_PIPE: u32 = 1;
#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
const MONOCP_FD_WASI_FD: u32 = 2;
#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
const MONOCP_FD_CLOSE: u32 = 3;

#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
pub(crate) fn execute_external_command(
    context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    executable_path: &str,
    _process_group_id: Option<i32>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    // Filter out the args; we only want strings.
    let cmd_args = args
        .iter()
        .filter_map(|e| {
            if let CommandArg::String(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let command = Path::new(executable_path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_else(|| context.command_name.as_str())
        .to_string();

    let mut argv: Vec<u8> = Vec::new();
    argv.extend_from_slice(command.as_bytes());
    argv.push(0);
    for arg in &cmd_args {
        argv.extend_from_slice(arg.as_bytes());
        argv.push(0);
    }

    let cwd = context.shell.working_dir().to_string_lossy().into_owned();

    // Capture a full fd table snapshot so pipe/file handles remain alive for the duration
    // of the host exec call.
    let fd_table = context.params.fd_table_snapshot(context.shell);

    let mut fds: Vec<MonocpFdSpec> = Vec::with_capacity(fd_table.len());
    for (fd, entry) in &fd_table {
        if *fd < 0 {
            continue;
        }
        let child_fd = *fd as u32;
        match entry {
            Some(file) => {
                if let Some(handle) = file.pipe_handle() {
                    fds.push(MonocpFdSpec {
                        child_fd,
                        kind: MONOCP_FD_PIPE,
                        value: handle,
                        flags: 0,
                    });
                    continue;
                }

                #[cfg(target_os = "wasi")]
                if let Some(wasi_fd) = file.wasi_fd() {
                    fds.push(MonocpFdSpec {
                        child_fd,
                        kind: MONOCP_FD_WASI_FD,
                        value: wasi_fd,
                        flags: 0,
                    });
                    continue;
                }

                return Err(error::ErrorKind::NotSupportedOnThisPlatform(
                    "propagating non-file/pipe fds",
                )
                .into());
            }
            None => {
                fds.push(MonocpFdSpec {
                    child_fd,
                    kind: MONOCP_FD_CLOSE,
                    value: 0,
                    flags: 0,
                });
            }
        }
    }

    let to_u32 = |value: usize| -> Result<u32, error::Error> {
        u32::try_from(value).map_err(|_| {
            error::ErrorKind::FailedToExecuteCommand(
                command.clone(),
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "exec context too large"),
            )
            .into()
        })
    };

    let ctx = MonocpExecContextV2 {
        argv_ptr: to_u32(argv.as_ptr() as usize)?,
        argv_len: to_u32(argv.len())?,
        cwd_ptr: to_u32(cwd.as_ptr() as usize)?,
        cwd_len: to_u32(cwd.len())?,
        fds_ptr: to_u32(fds.as_ptr() as usize)?,
        fds_len: to_u32(fds.len())?,
    };

    // SAFETY: monocp_exec_with_context_v2 expects a valid context struct pointer and we keep
    // argv/cwd/fd-table buffers alive for the duration of the call.
    let exit_code = unsafe {
        monocp_exec_with_context_v2(
            std::ptr::from_ref(&ctx).cast::<u8>(),
            std::mem::size_of::<MonocpExecContextV2>(),
        )
    };

    let normalized = if exit_code < 0 {
        127_u8
    } else if exit_code > u8::MAX as i32 {
        (exit_code & 0xFF) as u8
    } else {
        exit_code as u8
    };

    Ok(ExecutionResult::new(normalized).into())
}

#[cfg(all(target_arch = "wasm32", target_env = "p2"))]
pub(crate) fn execute_external_command(
    context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    executable_path: &str,
    _process_group_id: Option<i32>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    use crate::sys::wasm::monocp_p2::monocp::host::monocp;
    use std::collections::HashMap;

    // Filter out the args; we only want strings.
    let cmd_args = args
        .iter()
        .filter_map(|e| {
            if let CommandArg::String(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let command = Path::new(executable_path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_else(|| context.command_name.as_str())
        .to_string();

    let mut argv: Vec<String> = Vec::with_capacity(cmd_args.len() + 1);
    argv.push(command.clone());
    for arg in &cmd_args {
        argv.push(arg.to_string());
    }

    let cwd = context.shell.working_dir().to_string_lossy().into_owned();

    // Capture a full fd table snapshot so pipe/file handles remain alive for the duration
    // of the host exec call.
    let fd_table = context.params.fd_table_snapshot(context.shell);

    let mut seen_files: HashMap<usize, u32> = HashMap::new();

    let mut fds: Vec<monocp::FdBinding> = Vec::with_capacity(fd_table.len());
    for (fd, entry) in &fd_table {
        if *fd < 0 {
            continue;
        }
        let child_fd = *fd as u32;

        let target = match entry {
            None => monocp::FdTarget::Close,
            Some(file) => {
                // Pipes
                if let Some(handle) = file.pipe_handle() {
                    match file {
                        OpenFile::PipeReader(_) => monocp::FdTarget::PipeRead(handle),
                        OpenFile::PipeWriter(_) => monocp::FdTarget::PipeWrite(handle),
                        _ => {
                            return Err(error::ErrorKind::NotSupportedOnThisPlatform(
                                "propagating non-directional pipe handle",
                            )
                            .into());
                        }
                    }
                } else if let Some(id) = file.wasi_file_id() {
                    if let Some(existing_fd) = seen_files.get(&id) {
                        monocp::FdTarget::Dup(*existing_fd)
                    } else if let Some((path, meta)) = file.wasi_file_meta() {
                        let path_str = path.to_string_lossy().into_owned();
                        let target = if meta.write {
                            monocp::FdTarget::FileWrite(monocp::FileWriteTarget {
                                path: path_str,
                                append: meta.append,
                            })
                        } else {
                            monocp::FdTarget::FileRead(path_str)
                        };
                        seen_files.insert(id, child_fd);
                        target
                    } else {
                        return Err(error::ErrorKind::NotSupportedOnThisPlatform(
                            "propagating anonymous WASI file",
                        )
                        .into());
                    }
                } else {
                    // stdio or unsupported
                    match file {
                        OpenFile::Stdin(_) | OpenFile::Stdout(_) | OpenFile::Stderr(_) => {
                            monocp::FdTarget::Inherit
                        }
                        _ => {
                            return Err(error::ErrorKind::NotSupportedOnThisPlatform(
                                "propagating non-file/pipe fds",
                            )
                            .into());
                        }
                    }
                }
            }
        };

        fds.push(monocp::FdBinding {
            fd: child_fd,
            target,
        });
    }

    let child_handle = monocp::spawn_with_context(&argv, &cwd, &fds).map_err(|msg| {
        error::ErrorKind::FailedToExecuteCommand(command.clone(), std::io::Error::other(msg))
    })?;

    Ok(ExecutionSpawnResult::started_host_child(child_handle))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn execute_external_command(
    context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    executable_path: &str,
    process_group_id: Option<i32>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    // Filter out the args; we only want strings.
    let cmd_args = args
        .iter()
        .filter_map(|e| {
            if let CommandArg::String(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    // Before we lose ownership of the open files, figure out if stdin will be a terminal.
    let child_stdin_is_terminal = context
        .try_fd(openfiles::OpenFiles::STDIN_FD)
        .is_some_and(|f| f.is_terminal());

    // Figure out if we should be setting up a new process group.
    let new_pg = context.should_cmd_lead_own_process_group();

    // Compose the std::process::Command that encapsulates what we want to launch.
    #[allow(unused_mut, reason = "only mutated on unix platforms")]
    let mut cmd = compose_std_command(
        &context,
        executable_path,
        context.command_name.as_str(),
        cmd_args.as_slice(),
        false, /* empty environment? */
    )?;

    // Set up process group state.
    if new_pg {
        // Check if we'll be doing terminal control setup (which includes setsid)
        if child_stdin_is_terminal && context.shell.options().external_cmd_leads_session {
            // Don't set process_group(0) - setsid() in pre_exec will handle it
            cmd.lead_session();
        } else {
            // Normal case: create new process group in current session
            cmd.process_group(0);
            if child_stdin_is_terminal {
                cmd.take_foreground();
            }
        }
    } else {
        // We need to join an established process group.
        if let Some(pgid) = process_group_id {
            cmd.process_group(pgid);
        }
    }

    // When tracing is enabled, report.
    tracing::debug!(
        target: trace_categories::COMMANDS,
        "Spawning: cmd='{} {}'",
        cmd.get_program().to_string_lossy().to_string(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .join(" ")
    );

    match sys::process::spawn(cmd) {
        Ok(child) => {
            // Retrieve the pid.
            #[expect(clippy::cast_possible_wrap)]
            let pid = child.id().map(|id| id as i32);
            let mut actual_pgid = process_group_id;
            if let Some(pid) = &pid {
                if new_pg {
                    actual_pgid = Some(*pid);
                }
            } else {
                tracing::warn!("could not retrieve pid for child process");
            }

            Ok(ExecutionSpawnResult::StartedProcess(
                processes::ChildProcess::new(child, pid, actual_pgid),
            ))
        }
        Err(spawn_err) => {
            if context.shell.options().interactive {
                sys::terminal::move_self_to_foreground()?;
            }

            if spawn_err.kind() == std::io::ErrorKind::NotFound {
                if !context.shell.working_dir().exists() {
                    Err(
                        error::ErrorKind::WorkingDirMissing(context.shell.working_dir().to_owned())
                            .into(),
                    )
                } else {
                    Err(error::ErrorKind::CommandNotFound(context.command_name).into())
                }
            } else {
                Err(
                    error::ErrorKind::FailedToExecuteCommand(context.command_name, spawn_err)
                        .into(),
                )
            }
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_env = "p1"))]
#[link(wasm_import_module = "monocp")]
unsafe extern "C" {
    #[link_name = "exec_with_context_v2"]
    fn monocp_exec_with_context_v2(ctx_ptr: *const u8, ctx_len: usize) -> i32;
}

async fn execute_builtin_command<SE: extensions::ShellExtensions>(
    builtin: &builtins::Registration<SE>,
    context: ExecutionContext<'_, SE>,
    args: Vec<CommandArg>,
) -> Result<ExecutionResult, error::Error> {
    // In POSIX mode, special builtins that return errors are to be treated as fatal.
    let mark_errors_fatal = builtin.special_builtin && context.shell.options().posix_mode;

    match (builtin.execute_func)(context, args).await {
        Ok(result) => Ok(result),
        Err(e) => {
            // Broken pipe errors should silently return the appropriate exit code
            if let Some(io_err) = e.as_io_error() {
                if io_err.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(ExecutionExitCode::from(io_err).into());
                }
            }

            Err(if mark_errors_fatal { e.into_fatal() } else { e })
        }
    }
}

pub(crate) async fn invoke_shell_function(
    function: functions::Registration,
    mut context: ExecutionContext<'_, impl extensions::ShellExtensions>,
    args: &[CommandArg],
) -> Result<ExecutionSpawnResult, error::Error> {
    let ast::FunctionBody(body, redirects) = &function.definition().body;

    // Apply any redirects specified at function definition-time.
    if let Some(redirects) = redirects {
        for redirect in &redirects.0 {
            interp::setup_redirect(context.shell, &mut context.params, redirect).await?;
        }
    }

    let positional_args = args.iter().map(|a| a.to_string());

    // Pass through open files.
    let params = context.params.clone();

    // Note that we're going deeper. Once we do this, we need to make sure we don't bail early
    // before "exiting" the function.
    context.shell.enter_function(
        context.command_name.as_str(),
        &function,
        positional_args,
        &context.params,
    )?;

    // Invoke the function.
    let result = body.execute(context.shell, &params).await;

    // Clean up parameters so any owned files are closed.
    drop(params);

    // We've come back out, reflect it.
    context.shell.leave_function()?;

    // Get the actual execution result from the body of the function.
    let mut result = result?;

    // Handle control-flow.
    match result.next_control_flow {
        ExecutionControlFlow::BreakLoop { .. } | ExecutionControlFlow::ContinueLoop { .. } => {
            return error::unimp("break or continue returned from function invocation");
        }
        ExecutionControlFlow::ReturnFromFunctionOrScript => {
            // It's now been handled.
            result.next_control_flow = ExecutionControlFlow::Normal;
        }
        _ => {}
    }

    Ok(result.into())
}

pub(crate) async fn invoke_command_in_subshell_and_get_output(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    s: String,
) -> Result<String, error::Error> {
    // Instantiate a subshell to run the command in.
    let mut subshell = shell.clone();

    // Command substitutions don't inherit errexit by default. Only inherit it when
    // command_subst_inherits_errexit is enabled, otherwise disable errexit in the subshell.
    if !shell.options().command_subst_inherits_errexit {
        subshell.options_mut().exit_on_nonzero_command_exit = false;
    }

    // Get our own set of parameters we can customize and use.
    let mut params = params.clone();
    params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // Set up pipe so we can read the output.
    let (reader, writer) = sys::pipes::pipe()?;
    params.set_fd(OpenFiles::STDOUT_FD, writer.into());

    #[cfg(target_arch = "wasm32")]
    {
        // On wasm32, tokio::spawn_blocking may not be supported by the runtime
        // (e.g., no worker-thread support). Run the substitution command inline,
        // then read from the pipe once the writer has been dropped.
        let cmd_result = run_substitution_command(subshell, params, s).await?;

        // Extract output.
        let output_str = std::io::read_to_string(reader)?;

        // Store the status.
        shell.set_last_exit_status(cmd_result.exit_code.into());

        return Ok(output_str);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Start the execution of the command, but don't wait for it to
        // complete. In case the command generates lots of output, we
        // need to start reading in parallel so the command doesn't block
        // when the pipe's buffer fills up. We pass ownership of the
        // subshell and params to run_substitution_command; we must
        // ensure that they're both dropped by the time this call
        // returns (so they're not holding onto the write end of the pipe).
        let cmd_join_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(run_substitution_command(subshell, params, s))
        });

        // Extract output.
        let output_str = std::io::read_to_string(reader)?;

        // Now observe the command's completion.
        let run_result = cmd_join_handle.await?;
        let cmd_result = run_result?;

        // Store the status.
        shell.set_last_exit_status(cmd_result.exit_code.into());

        Ok(output_str)
    }
}

async fn run_substitution_command(
    mut shell: Shell<impl extensions::ShellExtensions>,
    mut params: ExecutionParameters,
    command: String,
) -> Result<ExecutionResult, error::Error> {
    // Parse the string into a whole shell program.
    let parse_result = shell.parse_string(command);

    // Check for a command that is only an input redirection ("< file").
    // If detected, emulate `cat file` to stdout and return immediately.
    // If we failed to parse, then we'll fall below and handle it there.
    if let Ok(program) = &parse_result {
        if let Some(redir) = try_unwrap_bare_input_redir_program(program) {
            interp::setup_redirect(&mut shell, &mut params, redir).await?;
            std::io::copy(&mut params.stdin(&shell), &mut params.stdout(&shell))?;
            return Ok(ExecutionResult::new(0));
        }
    }

    // TODO(source-info): review this
    let source_info = crate::SourceInfo::from("main");

    // Handle the parse result using default shell behavior.
    shell
        .run_parsed_result(parse_result, &source_info, &params)
        .await
}

// Detects a subshell command that consists solely of a single input redirection
// (e.g., "< file"), returning the IoRedirect when present.
fn try_unwrap_bare_input_redir_program(program: &ast::Program) -> Option<&ast::IoRedirect> {
    // We're looking for exactly one complete command...
    let [complete] = program.complete_commands.as_slice() else {
        return None;
    };

    // ...a single list item...
    let ast::CompoundList(items) = complete;
    let [item] = items.as_slice() else {
        return None;
    };

    // ...with a single pipeline (no && or || chaining)...
    let and_or = &item.0;
    if !and_or.additional.is_empty() {
        return None;
    }

    // ...not negated...
    let pipeline = &and_or.first;
    if pipeline.bang {
        return None;
    }

    // ...with a single command in the pipeline...
    let [ast::Command::Simple(simple_cmd)] = pipeline.seq.as_slice() else {
        return None;
    };

    // ...with no program word/name and no suffix...
    if simple_cmd.word_or_name.is_some() || simple_cmd.suffix.is_some() {
        return None;
    }

    // ...and exactly one prefix containing an I/O redirect...
    let prefix = simple_cmd.prefix.as_ref()?;
    let [ast::CommandPrefixOrSuffixItem::IoRedirect(redir)] = prefix.0.as_slice() else {
        return None;
    };

    // ...that is a file input redirection to a filename, targeting stdin.
    match redir {
        ast::IoRedirect::File(
            fd,
            ast::IoFileRedirectKind::Read,
            ast::IoFileRedirectTarget::Filename(..),
        ) if fd.is_none_or(|fd| fd == openfiles::OpenFiles::STDIN_FD) => Some(redir),
        _ => None,
    }
}
