//! Builder system for configuring a [`WasiState`] and creating it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use rand::Rng;
use thiserror::Error;
use virtual_fs::{ArcFile, FsError, TmpFileSystem, VirtualFile};
use wasmer::{AsStoreMut, Instance, Module, RuntimeError, Store};
use wasmer_wasix_types::wasi::{Errno, ExitCode};

#[cfg(feature = "sys")]
use crate::PluggableRuntime;
use crate::{
    bin_factory::{BinFactory, BinaryPackage},
    capabilities::Capabilities,
    fs::{WasiFs, WasiFsRoot, WasiInodes},
    os::task::control_plane::{ControlPlaneConfig, ControlPlaneError, WasiControlPlane},
    state::WasiState,
    syscalls::types::{__WASI_STDERR_FILENO, __WASI_STDIN_FILENO, __WASI_STDOUT_FILENO},
    RewindState, Runtime, WasiEnv, WasiError, WasiFunctionEnv, WasiRuntimeError,
};

use super::env::WasiEnvInit;

/// Builder API for configuring a [`WasiEnv`] environment needed to run WASI modules.
///
/// Usage:
/// ```no_run
/// # use wasmer_wasix::{WasiEnv, WasiStateCreationError};
/// # fn main() -> Result<(), WasiStateCreationError> {
/// let mut state_builder = WasiEnv::builder("wasi-prog-name");
/// state_builder
///    .env("ENV_VAR", "ENV_VAL")
///    .arg("--verbose")
///    .preopen_dir("src")?
///    .map_dir("name_wasi_sees", "path/on/host/fs")?
///    .build_init()?;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct WasiEnvBuilder {
    /// Command line arguments.
    pub(super) args: Vec<String>,
    /// Environment variables.
    pub(super) envs: Vec<(String, Vec<u8>)>,
    /// Pre-opened directories that will be accessible from WASI.
    pub(super) preopens: Vec<PreopenedDir>,
    /// Pre-opened virtual directories that will be accessible from WASI.
    vfs_preopens: Vec<String>,
    #[allow(clippy::type_complexity)]
    pub(super) setup_fs_fn:
        Option<Box<dyn Fn(&WasiInodes, &mut WasiFs) -> Result<(), String> + Send>>,
    pub(super) stdout: Option<Box<dyn VirtualFile + Send + Sync + 'static>>,
    pub(super) stderr: Option<Box<dyn VirtualFile + Send + Sync + 'static>>,
    pub(super) stdin: Option<Box<dyn VirtualFile + Send + Sync + 'static>>,
    pub(super) fs: Option<WasiFsRoot>,
    pub(super) runtime: Option<Arc<dyn crate::Runtime + Send + Sync + 'static>>,

    /// List of webc dependencies to be injected.
    pub(super) uses: Vec<BinaryPackage>,

    /// List of host commands to map into the WASI instance.
    pub(super) map_commands: HashMap<String, PathBuf>,

    pub(super) capabilites: Capabilities,
}

impl std::fmt::Debug for WasiEnvBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: update this when stable
        f.debug_struct("WasiEnvBuilder")
            .field("args", &self.args)
            .field("envs", &self.envs)
            .field("preopens", &self.preopens)
            .field("uses", &self.uses)
            .field("setup_fs_fn exists", &self.setup_fs_fn.is_some())
            .field("stdout_override exists", &self.stdout.is_some())
            .field("stderr_override exists", &self.stderr.is_some())
            .field("stdin_override exists", &self.stdin.is_some())
            .field("runtime_override_exists", &self.runtime.is_some())
            .finish()
    }
}

/// Error type returned when bad data is given to [`WasiEnvBuilder`].
#[derive(Error, Debug, PartialEq, Eq)]
pub enum WasiStateCreationError {
    #[error("bad environment variable format: `{0}`")]
    EnvironmentVariableFormatError(String),
    #[error("argument contains null byte: `{0}`")]
    ArgumentContainsNulByte(String),
    #[error("preopened directory not found: `{0}`")]
    PreopenedDirectoryNotFound(PathBuf),
    #[error("preopened directory error: `{0}`")]
    PreopenedDirectoryError(String),
    #[error("mapped dir alias has wrong format: `{0}`")]
    MappedDirAliasFormattingError(String),
    #[error("wasi filesystem creation error: `{0}`")]
    WasiFsCreationError(String),
    #[error("wasi filesystem setup error: `{0}`")]
    WasiFsSetupError(String),
    #[error(transparent)]
    FileSystemError(#[from] FsError),
    #[error("wasi inherit error: `{0}`")]
    WasiInheritError(String),
    #[error("wasi include package: `{0}`")]
    WasiIncludePackageError(String),
    #[error("control plane error")]
    ControlPlane(#[from] ControlPlaneError),
}

fn validate_mapped_dir_alias(alias: &str) -> Result<(), WasiStateCreationError> {
    if !alias.bytes().all(|b| b != b'\0') {
        return Err(WasiStateCreationError::MappedDirAliasFormattingError(
            format!("Alias \"{}\" contains a nul byte", alias),
        ));
    }

    Ok(())
}

pub type SetupFsFn = Box<dyn Fn(&WasiInodes, &mut WasiFs) -> Result<(), String> + Send>;

// TODO add other WasiFS APIs here like swapping out stdout, for example (though we need to
// return stdout somehow, it's unclear what that API should look like)
impl WasiEnvBuilder {
    /// Creates an empty [`WasiEnvBuilder`].
    pub fn new(program_name: impl Into<String>) -> Self {
        WasiEnvBuilder {
            args: vec![program_name.into()],
            ..WasiEnvBuilder::default()
        }
    }

    /// Add an environment variable pair.
    ///
    /// Both the key and value of an environment variable must not
    /// contain a nul byte (`0x0`), and the key must not contain the
    /// `=` byte (`0x3d`).
    pub fn env<Key, Value>(mut self, key: Key, value: Value) -> Self
    where
        Key: AsRef<[u8]>,
        Value: AsRef<[u8]>,
    {
        self.add_env(key, value);
        self
    }

    /// Add an environment variable pair.
    ///
    /// Both the key and value of an environment variable must not
    /// contain a nul byte (`0x0`), and the key must not contain the
    /// `=` byte (`0x3d`).
    pub fn add_env<Key, Value>(&mut self, key: Key, value: Value)
    where
        Key: AsRef<[u8]>,
        Value: AsRef<[u8]>,
    {
        self.envs.push((
            String::from_utf8_lossy(key.as_ref()).to_string(),
            value.as_ref().to_vec(),
        ));
    }

    /// Add multiple environment variable pairs.
    ///
    /// Both the key and value of the environment variables must not
    /// contain a nul byte (`0x0`), and the key must not contain the
    /// `=` byte (`0x3d`).
    pub fn envs<I, Key, Value>(mut self, env_pairs: I) -> Self
    where
        I: IntoIterator<Item = (Key, Value)>,
        Key: AsRef<[u8]>,
        Value: AsRef<[u8]>,
    {
        self.add_envs(env_pairs);

        self
    }

    /// Add multiple environment variable pairs.
    ///
    /// Both the key and value of the environment variables must not
    /// contain a nul byte (`0x0`), and the key must not contain the
    /// `=` byte (`0x3d`).
    pub fn add_envs<I, Key, Value>(&mut self, env_pairs: I)
    where
        I: IntoIterator<Item = (Key, Value)>,
        Key: AsRef<[u8]>,
        Value: AsRef<[u8]>,
    {
        for (key, value) in env_pairs {
            self.add_env(key, value);
        }
    }

    /// Get a reference to the configured environment variables.
    pub fn get_env(&self) -> &[(String, Vec<u8>)] {
        &self.envs
    }

    /// Get a mutable reference to the configured environment variables.
    pub fn get_env_mut(&mut self) -> &mut Vec<(String, Vec<u8>)> {
        &mut self.envs
    }

    /// Add an argument.
    ///
    /// Arguments must not contain the nul (0x0) byte
    // TODO: should take Into<Vec<u8>>
    pub fn arg<V>(mut self, arg: V) -> Self
    where
        V: AsRef<[u8]>,
    {
        self.add_arg(arg);
        self
    }

    /// Add an argument.
    ///
    /// Arguments must not contain the nul (0x0) byte.
    // TODO: should take Into<Vec<u8>>
    pub fn add_arg<V>(&mut self, arg: V)
    where
        V: AsRef<[u8]>,
    {
        self.args
            .push(String::from_utf8_lossy(arg.as_ref()).to_string());
    }

    /// Add multiple arguments.
    ///
    /// Arguments must not contain the nul (0x0) byte
    pub fn args<I, Arg>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = Arg>,
        Arg: AsRef<[u8]>,
    {
        self.add_args(args);

        self
    }

    /// Add multiple arguments.
    ///
    /// Arguments must not contain the nul (0x0) byte
    pub fn add_args<I, Arg>(&mut self, args: I)
    where
        I: IntoIterator<Item = Arg>,
        Arg: AsRef<[u8]>,
    {
        for arg in args {
            self.add_arg(arg);
        }
    }

    /// Get a reference to the configured arguments.
    pub fn get_args(&self) -> &[String] {
        &self.args
    }

    /// Get a mutable reference to the configured arguments.
    pub fn get_args_mut(&mut self) -> &mut Vec<String> {
        &mut self.args
    }

    /// Adds a container this module inherits from.
    ///
    /// This will make all of the container's files and commands available to the
    /// resulting WASI instance.
    pub fn use_webc(mut self, pkg: BinaryPackage) -> Self {
        self.add_webc(pkg);
        self
    }

    /// Adds a container this module inherits from.
    ///
    /// This will make all of the container's files and commands available to the
    /// resulting WASI instance.
    pub fn add_webc(&mut self, pkg: BinaryPackage) -> &mut Self {
        self.uses.push(pkg);
        self
    }

    /// Adds a list of other containers this module inherits from.
    ///
    /// This will make all of the container's files and commands available to the
    /// resulting WASI instance.
    pub fn uses<I>(mut self, uses: I) -> Self
    where
        I: IntoIterator<Item = BinaryPackage>,
    {
        for pkg in uses {
            self.add_webc(pkg);
        }
        self
    }

    /// Map an atom to a local binary
    #[cfg(feature = "sys")]
    pub fn map_command<Name, Target>(mut self, name: Name, target: Target) -> Self
    where
        Name: AsRef<str>,
        Target: AsRef<str>,
    {
        let path_buf = PathBuf::from(target.as_ref().to_string());
        self.map_commands
            .insert(name.as_ref().to_string(), path_buf);
        self
    }

    /// Maps a series of atoms to the local binaries
    #[cfg(feature = "sys")]
    pub fn map_commands<I, Name, Target>(mut self, map_commands: I) -> Self
    where
        I: IntoIterator<Item = (Name, Target)>,
        Name: AsRef<str>,
        Target: AsRef<str>,
    {
        map_commands.into_iter().for_each(|(name, target)| {
            let path_buf = PathBuf::from(target.as_ref().to_string());
            self.map_commands
                .insert(name.as_ref().to_string(), path_buf);
        });
        self
    }

    /// Preopen a directory
    ///
    /// This opens the given directory at the virtual root, `/`, and allows
    /// the WASI module to read and write to the given directory.
    pub fn preopen_dir<P>(mut self, po_dir: P) -> Result<Self, WasiStateCreationError>
    where
        P: AsRef<Path>,
    {
        self.add_preopen_dir(po_dir)?;
        Ok(self)
    }

    /// Adds a preopen a directory
    ///
    /// This opens the given directory at the virtual root, `/`, and allows
    /// the WASI module to read and write to the given directory.
    pub fn add_preopen_dir<P>(&mut self, po_dir: P) -> Result<(), WasiStateCreationError>
    where
        P: AsRef<Path>,
    {
        let mut pdb = PreopenDirBuilder::new();
        let path = po_dir.as_ref();
        pdb.directory(path).read(true).write(true).create(true);
        let preopen = pdb.build()?;

        self.preopens.push(preopen);

        Ok(())
    }

    /// Preopen multiple directories.
    ///
    /// This opens the given directories at the virtual root, `/`, and allows
    /// the WASI module to read and write to the given directory.
    pub fn preopen_dirs<I, P>(mut self, dirs: I) -> Result<Self, WasiStateCreationError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        for po_dir in dirs {
            self.add_preopen_dir(po_dir)?;
        }

        Ok(self)
    }

    /// Preopen a directory and configure it.
    ///
    /// Usage:
    ///
    /// ```no_run
    /// # use wasmer_wasix::{WasiEnv, WasiStateCreationError};
    /// # fn main() -> Result<(), WasiStateCreationError> {
    /// WasiEnv::builder("program_name")
    ///    .preopen_build(|p| p.directory("src").read(true).write(true).create(true))?
    ///    .preopen_build(|p| p.directory(".").alias("dot").read(true))?
    ///    .build_init()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn preopen_build<F>(mut self, inner: F) -> Result<Self, WasiStateCreationError>
    where
        F: Fn(&mut PreopenDirBuilder) -> &mut PreopenDirBuilder,
    {
        self.add_preopen_build(inner)?;
        Ok(self)
    }

    /// Preopen a directory and configure it.
    ///
    /// Usage:
    ///
    /// ```no_run
    /// # use wasmer_wasix::{WasiEnv, WasiStateCreationError};
    /// # fn main() -> Result<(), WasiStateCreationError> {
    /// WasiEnv::builder("program_name")
    ///    .preopen_build(|p| p.directory("src").read(true).write(true).create(true))?
    ///    .preopen_build(|p| p.directory(".").alias("dot").read(true))?
    ///    .build_init()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_preopen_build<F>(&mut self, inner: F) -> Result<(), WasiStateCreationError>
    where
        F: Fn(&mut PreopenDirBuilder) -> &mut PreopenDirBuilder,
    {
        let mut pdb = PreopenDirBuilder::new();
        let po_dir = inner(&mut pdb).build()?;

        self.preopens.push(po_dir);

        Ok(())
    }

    /// Preopen the given directories from the
    /// Virtual FS.
    pub fn preopen_vfs_dirs<I>(&mut self, po_dirs: I) -> Result<&mut Self, WasiStateCreationError>
    where
        I: IntoIterator<Item = String>,
    {
        for po_dir in po_dirs {
            self.vfs_preopens.push(po_dir);
        }

        Ok(self)
    }

    /// Preopen a directory with a different name exposed to the WASI.
    pub fn map_dir<P>(mut self, alias: &str, po_dir: P) -> Result<Self, WasiStateCreationError>
    where
        P: AsRef<Path>,
    {
        self.add_map_dir(alias, po_dir)?;
        Ok(self)
    }

    /// Preopen a directory with a different name exposed to the WASI.
    pub fn add_map_dir<P>(&mut self, alias: &str, po_dir: P) -> Result<(), WasiStateCreationError>
    where
        P: AsRef<Path>,
    {
        let mut pdb = PreopenDirBuilder::new();
        let path = po_dir.as_ref();
        pdb.directory(path)
            .alias(alias)
            .read(true)
            .write(true)
            .create(true);
        let preopen = pdb.build()?;

        self.preopens.push(preopen);

        Ok(())
    }

    /// Preopen directorys with a different names exposed to the WASI.
    pub fn map_dirs<I, P>(mut self, mapped_dirs: I) -> Result<Self, WasiStateCreationError>
    where
        I: IntoIterator<Item = (String, P)>,
        P: AsRef<Path>,
    {
        for (alias, dir) in mapped_dirs {
            self.add_map_dir(&alias, dir)?;
        }

        Ok(self)
    }

    /// Overwrite the default WASI `stdout`, if you want to hold on to the
    /// original `stdout` use [`WasiFs::swap_file`] after building.
    pub fn stdout(mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.stdout = Some(new_file);

        self
    }

    /// Overwrite the default WASI `stdout`, if you want to hold on to the
    /// original `stdout` use [`WasiFs::swap_file`] after building.
    pub fn set_stdout(&mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) {
        self.stdout = Some(new_file);
    }

    /// Overwrite the default WASI `stderr`, if you want to hold on to the
    /// original `stderr` use [`WasiFs::swap_file`] after building.
    pub fn stderr(mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.set_stderr(new_file);
        self
    }

    /// Overwrite the default WASI `stderr`, if you want to hold on to the
    /// original `stderr` use [`WasiFs::swap_file`] after building.
    pub fn set_stderr(&mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) {
        self.stderr = Some(new_file);
    }

    /// Overwrite the default WASI `stdin`, if you want to hold on to the
    /// original `stdin` use [`WasiFs::swap_file`] after building.
    pub fn stdin(mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.stdin = Some(new_file);

        self
    }

    /// Overwrite the default WASI `stdin`, if you want to hold on to the
    /// original `stdin` use [`WasiFs::swap_file`] after building.
    pub fn set_stdin(&mut self, new_file: Box<dyn VirtualFile + Send + Sync + 'static>) {
        self.stdin = Some(new_file);
    }

    /// Sets the FileSystem to be used with this WASI instance.
    ///
    /// This is usually used in case a custom `virtual_fs::FileSystem` is needed.
    pub fn fs(mut self, fs: Box<dyn virtual_fs::FileSystem + Send + Sync>) -> Self {
        self.set_fs(fs);
        self
    }

    pub fn set_fs(&mut self, fs: Box<dyn virtual_fs::FileSystem + Send + Sync>) {
        self.fs = Some(WasiFsRoot::Backing(Arc::new(fs)));
    }

    /// Sets a new sandbox FileSystem to be used with this WASI instance.
    ///
    /// This is usually used in case a custom `virtual_fs::FileSystem` is needed.
    pub fn sandbox_fs(mut self, fs: TmpFileSystem) -> Self {
        self.fs = Some(WasiFsRoot::Sandbox(Arc::new(fs)));
        self
    }

    /// Configure the WASI filesystem before running.
    // TODO: improve ergonomics on this function
    pub fn setup_fs(mut self, setup_fs_fn: SetupFsFn) -> Self {
        self.setup_fs_fn = Some(setup_fs_fn);

        self
    }

    /// Sets the WASI runtime implementation and overrides the default
    /// implementation
    pub fn runtime(mut self, runtime: Arc<dyn Runtime + Send + Sync>) -> Self {
        self.set_runtime(runtime);
        self
    }

    pub fn set_runtime(&mut self, runtime: Arc<dyn Runtime + Send + Sync>) {
        self.runtime = Some(runtime);
    }

    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.set_capabilities(capabilities);
        self
    }

    pub fn capabilities_mut(&mut self) -> &mut Capabilities {
        &mut self.capabilites
    }

    pub fn set_capabilities(&mut self, capabilities: Capabilities) {
        self.capabilites = capabilities;
    }

    /// Consumes the [`WasiEnvBuilder`] and produces a [`WasiEnvInit`], which
    /// can be used to construct a new [`WasiEnv`].
    ///
    /// Returns the error from `WasiFs::new` if there's an error
    ///
    /// NOTE: You should prefer to not work directly with [`WasiEnvInit`].
    /// Use [`WasiEnvBuilder::run`] or [`WasiEnvBuilder::run_with_store`] instead
    /// to ensure proper invokation of WASI modules.
    pub fn build_init(mut self) -> Result<WasiEnvInit, WasiStateCreationError> {
        for arg in self.args.iter() {
            for b in arg.as_bytes().iter() {
                if *b == 0 {
                    return Err(WasiStateCreationError::ArgumentContainsNulByte(arg.clone()));
                }
            }
        }

        enum InvalidCharacter {
            Nul,
            Equal,
        }

        for (env_key, env_value) in self.envs.iter() {
            match env_key.as_bytes().iter().find_map(|&ch| {
                if ch == 0 {
                    Some(InvalidCharacter::Nul)
                } else if ch == b'=' {
                    Some(InvalidCharacter::Equal)
                } else {
                    None
                }
            }) {
                Some(InvalidCharacter::Nul) => {
                    return Err(WasiStateCreationError::EnvironmentVariableFormatError(
                        format!("found nul byte in env var key \"{}\" (key=value)", env_key),
                    ))
                }

                Some(InvalidCharacter::Equal) => {
                    return Err(WasiStateCreationError::EnvironmentVariableFormatError(
                        format!(
                            "found equal sign in env var key \"{}\" (key=value)",
                            env_key
                        ),
                    ))
                }

                None => (),
            }

            if env_value.iter().any(|&ch| ch == 0) {
                return Err(WasiStateCreationError::EnvironmentVariableFormatError(
                    format!(
                        "found nul byte in env var value \"{}\" (key=value)",
                        String::from_utf8_lossy(env_value),
                    ),
                ));
            }
        }

        // TODO: must be used! (runtime was removed from env, must ensure configured runtime is used)
        // // Get a reference to the runtime
        // let runtime = self
        //     .runtime
        //     .clone()
        //     .unwrap_or_else(|| Arc::new(PluggableRuntimeImplementation::default()));

        // Determine the STDIN
        let stdin: Box<dyn VirtualFile + Send + Sync + 'static> = self
            .stdin
            .take()
            .unwrap_or_else(|| Box::new(ArcFile::new(Box::<super::Stdin>::default())));

        let fs_backing = self
            .fs
            .take()
            .unwrap_or_else(|| WasiFsRoot::Sandbox(Arc::new(TmpFileSystem::new())));

        // self.preopens are checked in [`PreopenDirBuilder::build`]
        let inodes = crate::state::WasiInodes::new();
        let wasi_fs = {
            // self.preopens are checked in [`PreopenDirBuilder::build`]
            let mut wasi_fs =
                WasiFs::new_with_preopen(&inodes, &self.preopens, &self.vfs_preopens, fs_backing)
                    .map_err(WasiStateCreationError::WasiFsCreationError)?;

            // set up the file system, overriding base files and calling the setup function
            wasi_fs
                .swap_file(__WASI_STDIN_FILENO, stdin)
                .map_err(WasiStateCreationError::FileSystemError)?;

            if let Some(stdout_override) = self.stdout.take() {
                wasi_fs
                    .swap_file(__WASI_STDOUT_FILENO, stdout_override)
                    .map_err(WasiStateCreationError::FileSystemError)?;
            }

            if let Some(stderr_override) = self.stderr.take() {
                wasi_fs
                    .swap_file(__WASI_STDERR_FILENO, stderr_override)
                    .map_err(WasiStateCreationError::FileSystemError)?;
            }

            if let Some(f) = &self.setup_fs_fn {
                f(&inodes, &mut wasi_fs).map_err(WasiStateCreationError::WasiFsSetupError)?;
            }
            wasi_fs
        };

        let envs = self
            .envs
            .into_iter()
            .map(|(key, value)| {
                let mut env = Vec::with_capacity(key.len() + value.len() + 1);
                env.extend_from_slice(key.as_bytes());
                env.push(b'=');
                env.extend_from_slice(&value);

                env
            })
            .collect();

        let state = WasiState {
            fs: wasi_fs,
            secret: rand::thread_rng().gen::<[u8; 32]>(),
            inodes,
            args: self.args.clone(),
            preopen: self.vfs_preopens.clone(),
            futexs: Default::default(),
            clock_offset: Default::default(),
            envs,
        };

        let runtime = self.runtime.unwrap_or_else(|| {
            #[cfg(feature = "sys-thread")]
            {
                Arc::new(PluggableRuntime::new(Arc::new(crate::runtime::task_manager::tokio::TokioTaskManager::shared())))
            }

            #[cfg(not(feature = "sys-thread"))]
            {
                panic!("this build does not support a default runtime - specify one with WasiEnvBuilder::runtime()");
            }
        });

        let uses = self.uses;
        let map_commands = self.map_commands;

        let bin_factory = BinFactory::new(runtime.clone());

        let capabilities = self.capabilites;

        let plane_config = ControlPlaneConfig {
            max_task_count: capabilities.threading.max_threads,
            enable_asynchronous_threading: capabilities.threading.enable_asynchronous_threading,
        };
        let control_plane = WasiControlPlane::new(plane_config);

        let init = WasiEnvInit {
            state,
            runtime,
            webc_dependencies: uses,
            mapped_commands: map_commands,
            control_plane,
            bin_factory,
            capabilities,
            memory_ty: None,
            process: None,
            thread: None,
            call_initialize: true,
            can_deep_sleep: false,
        };

        Ok(init)
    }

    #[allow(clippy::result_large_err)]
    pub fn build(self) -> Result<WasiEnv, WasiRuntimeError> {
        let init = self.build_init()?;
        WasiEnv::from_init(init)
    }

    /// Construct a [`WasiFunctionEnv`].
    ///
    /// NOTE: you still must call [`WasiFunctionEnv::initialize`] to make an
    /// instance usable.
    #[doc(hidden)]
    #[allow(clippy::result_large_err)]
    pub fn finalize(
        self,
        store: &mut impl AsStoreMut,
    ) -> Result<WasiFunctionEnv, WasiRuntimeError> {
        let init = self.build_init()?;
        let env = WasiEnv::from_init(init)?;
        let func_env = WasiFunctionEnv::new(store, env);
        Ok(func_env)
    }

    /// Consumes the [`WasiEnvBuilder`] and produces a [`WasiEnvInit`], which
    /// can be used to construct a new [`WasiEnv`].
    ///
    /// Returns the error from `WasiFs::new` if there's an error
    // FIXME: use a proper custom error type
    #[allow(clippy::result_large_err)]
    pub fn instantiate(
        self,
        module: Module,
        store: &mut impl AsStoreMut,
    ) -> Result<(Instance, WasiFunctionEnv), WasiRuntimeError> {
        let init = self.build_init()?;
        WasiEnv::instantiate(init, module, store)
    }

    #[allow(clippy::result_large_err)]
    pub fn run(self, module: Module) -> Result<(), WasiRuntimeError> {
        let mut store = wasmer::Store::default();
        self.run_with_store(module, &mut store)
    }

    #[allow(clippy::result_large_err)]
    pub fn run_with_store(self, module: Module, store: &mut Store) -> Result<(), WasiRuntimeError> {
        if self.capabilites.threading.enable_asynchronous_threading {
            tracing::warn!(
                "The enable_asynchronous_threading capability is enabled. Use WasiEnvBuilder::run_with_store_async() to avoid spurious errors.",
            );
        }

        let (instance, env) = self.instantiate(module, store)?;

        let start = instance.exports.get_function("_start")?;
        env.data(&store).thread.set_status_running();

        let result = crate::run_wasi_func_start(start, store);
        let (result, exit_code) = wasi_exit_code(result);

        let pid = env.data(&store).pid();
        let tid = env.data(&store).tid();
        tracing::trace!(
            %pid,
            %tid,
            %exit_code,
            error=result.as_ref().err().map(|e| e as &dyn std::error::Error),
            "main exit",
        );

        env.cleanup(store, Some(exit_code));

        result
    }

    /// Start the WASI executable with async threads enabled.
    #[allow(clippy::result_large_err)]
    pub fn run_with_store_async(
        self,
        module: Module,
        mut store: Store,
    ) -> Result<(), WasiRuntimeError> {
        let (_, env) = self.instantiate(module, &mut store)?;

        env.data(&store).thread.set_status_running();

        let tasks = env.data(&store).tasks().clone();
        let pid = env.data(&store).pid();
        let tid = env.data(&store).tid();

        // The return value is passed synchronously and will block until the result
        // is returned this is because the main thread can go into a deep sleep and
        // exit the dedicated thread
        let (tx, rx) = std::sync::mpsc::channel();

        tasks.task_dedicated(Box::new(move || {
            run_with_deep_sleep(store, None, env, tx);
        }))?;

        let result = rx.recv().expect(
            "main thread terminated without a result, this normally means a panic occurred",
        );
        let (result, exit_code) = wasi_exit_code(result);

        tracing::trace!(
            %pid,
            %tid,
            %exit_code,
            error=result.as_ref().err().map(|e| e as &dyn std::error::Error),
            "main exit",
        );

        result
    }
}

/// Extract the exit code from a `Result<(), WasiRuntimeError>`.
///
/// We need this because calling `exit(0)` inside a WASI program technically
/// triggers [`WasiError`] with an exit code of `0`, but the end user won't want
/// that treated as an error.
fn wasi_exit_code(
    mut result: Result<(), WasiRuntimeError>,
) -> (Result<(), WasiRuntimeError>, ExitCode) {
    let exit_code = match &result {
        Ok(_) => Errno::Success.into(),
        Err(err) => match err.as_exit_code() {
            Some(code) if code.is_success() => {
                // This is actually not an error, so we need to fix up the
                // result
                result = Ok(());
                Errno::Success.into()
            }
            Some(other) => other,
            None => Errno::Noexec.into(),
        },
    };

    (result, exit_code)
}

fn run_with_deep_sleep(
    mut store: Store,
    rewind_state: Option<(RewindState, Bytes)>,
    env: WasiFunctionEnv,
    sender: std::sync::mpsc::Sender<Result<(), WasiRuntimeError>>,
) {
    if let Some((rewind_state, rewind_result)) = rewind_state {
        tracing::trace!("Rewinding");
        let errno = if rewind_state.is_64bit {
            crate::rewind_ext::<wasmer_types::Memory64>(
                env.env.clone().into_mut(&mut store),
                rewind_state.memory_stack,
                rewind_state.rewind_stack,
                rewind_state.store_data,
                rewind_result,
            )
        } else {
            crate::rewind_ext::<wasmer_types::Memory32>(
                env.env.clone().into_mut(&mut store),
                rewind_state.memory_stack,
                rewind_state.rewind_stack,
                rewind_state.store_data,
                rewind_result,
            )
        };

        if errno != Errno::Success {
            let exit_code = ExitCode::from(errno);
            env.cleanup(&mut store, Some(exit_code));
            let _ = sender.send(Err(WasiRuntimeError::Wasi(WasiError::Exit(exit_code))));
            return;
        }
    }

    let instance = match env.data(&store).try_clone_instance() {
        Some(instance) => instance,
        None => {
            tracing::debug!("Unable to clone the instance");
            env.cleanup(&mut store, None);
            let _ = sender.send(Err(WasiRuntimeError::Wasi(WasiError::Exit(
                Errno::Noexec.into(),
            ))));
            return;
        }
    };

    let start = match instance.exports.get_function("_start") {
        Ok(start) => start,
        Err(e) => {
            tracing::debug!("Unable to get the _start function");
            env.cleanup(&mut store, None);
            let _ = sender.send(Err(e.into()));
            return;
        }
    };

    let result = start.call(&mut store, &[]);
    handle_result(store, env, result, sender);
}

fn handle_result(
    mut store: Store,
    env: WasiFunctionEnv,
    result: Result<Box<[wasmer::Value]>, RuntimeError>,
    sender: std::sync::mpsc::Sender<Result<(), WasiRuntimeError>>,
) {
    let result = match result.map_err(|e| e.downcast::<WasiError>()) {
        Err(Ok(WasiError::DeepSleep(work))) => {
            let pid = env.data(&store).pid();
            let tid = env.data(&store).tid();
            tracing::trace!(%pid, %tid, "entered a deep sleep");

            let tasks = env.data(&store).tasks().clone();
            let rewind = work.rewind;
            let respawn =
                move |ctx, store, res| run_with_deep_sleep(store, Some((rewind, res)), ctx, sender);

            // Spawns the WASM process after a trigger
            unsafe {
                tasks
                    .resume_wasm_after_poller(Box::new(respawn), env, store, work.trigger)
                    .unwrap();
            }

            return;
        }
        Ok(_) => Ok(()),
        Err(Ok(other)) => Err(other.into()),
        Err(Err(e)) => Err(e.into()),
    };

    let (result, exit_code) = wasi_exit_code(result);
    env.cleanup(&mut store, Some(exit_code));
    let _ = sender.send(result);
}

/// Builder for preopened directories.
#[derive(Debug, Default)]
pub struct PreopenDirBuilder {
    path: Option<PathBuf>,
    alias: Option<String>,
    read: bool,
    write: bool,
    create: bool,
}

/// The built version of `PreopenDirBuilder`
#[derive(Debug, Clone, Default)]
pub(crate) struct PreopenedDir {
    pub(crate) path: PathBuf,
    pub(crate) alias: Option<String>,
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) create: bool,
}

impl PreopenDirBuilder {
    /// Create an empty builder
    pub(crate) fn new() -> Self {
        PreopenDirBuilder::default()
    }

    /// Point the preopened directory to the path given by `po_dir`
    pub fn directory<FilePath>(&mut self, po_dir: FilePath) -> &mut Self
    where
        FilePath: AsRef<Path>,
    {
        let path = po_dir.as_ref();
        self.path = Some(path.to_path_buf());

        self
    }

    /// Make this preopened directory appear to the WASI program as `alias`
    pub fn alias(&mut self, alias: &str) -> &mut Self {
        // We mount at preopened dirs at `/` by default and multiple `/` in a row
        // are equal to a single `/`.
        let alias = alias.trim_start_matches('/');
        self.alias = Some(alias.to_string());

        self
    }

    /// Set read permissions affecting files in the directory
    pub fn read(&mut self, toggle: bool) -> &mut Self {
        self.read = toggle;

        self
    }

    /// Set write permissions affecting files in the directory
    pub fn write(&mut self, toggle: bool) -> &mut Self {
        self.write = toggle;

        self
    }

    /// Set create permissions affecting files in the directory
    ///
    /// Create implies `write` permissions
    pub fn create(&mut self, toggle: bool) -> &mut Self {
        self.create = toggle;
        if toggle {
            self.write = true;
        }

        self
    }

    pub(crate) fn build(&self) -> Result<PreopenedDir, WasiStateCreationError> {
        // ensure at least one is set
        if !(self.read || self.write || self.create) {
            return Err(WasiStateCreationError::PreopenedDirectoryError("Preopened directories must have at least one of read, write, create permissions set".to_string()));
        }

        if self.path.is_none() {
            return Err(WasiStateCreationError::PreopenedDirectoryError(
                "Preopened directories must point to a host directory".to_string(),
            ));
        }
        let path = self.path.clone().unwrap();

        /*
        if !path.exists() {
            return Err(WasiStateCreationError::PreopenedDirectoryNotFound(path));
        }
        */

        if let Some(alias) = &self.alias {
            validate_mapped_dir_alias(alias)?;
        }

        Ok(PreopenedDir {
            path,
            alias: self.alias.clone(),
            read: self.read,
            write: self.write,
            create: self.create,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn env_var_errors() {
        // `=` in the key is invalid.
        assert!(
            WasiEnv::builder("test_prog")
                .env("HOM=E", "/home/home")
                .build_init()
                .is_err(),
            "equal sign in key must be invalid"
        );

        // `\0` in the key is invalid.
        assert!(
            WasiEnvBuilder::new("test_prog")
                .env("HOME\0", "/home/home")
                .build_init()
                .is_err(),
            "nul in key must be invalid"
        );

        // `=` in the value is valid.
        assert!(
            WasiEnvBuilder::new("test_prog")
                .env("HOME", "/home/home=home")
                .build_init()
                .is_ok(),
            "equal sign in the value must be valid"
        );

        // `\0` in the value is invalid.
        assert!(
            WasiEnvBuilder::new("test_prog")
                .env("HOME", "/home/home\0")
                .build_init()
                .is_err(),
            "nul in value must be invalid"
        );
    }

    #[test]
    fn nul_character_in_args() {
        let output = WasiEnvBuilder::new("test_prog")
            .arg("--h\0elp")
            .build_init();
        let err = output.expect_err("should fail");
        assert!(matches!(
            err,
            WasiStateCreationError::ArgumentContainsNulByte(_)
        ));

        let output = WasiEnvBuilder::new("test_prog")
            .args(["--help", "--wat\0"])
            .build_init();
        let err = output.expect_err("should fail");
        assert!(matches!(
            err,
            WasiStateCreationError::ArgumentContainsNulByte(_)
        ));
    }
}
