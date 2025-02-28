#![allow(unused_imports)]
#![allow(dead_code)]

pub mod cconst;

use std::{
    collections::HashMap,
    io::Write,
    ops::{Deref, DerefMut},
    path::Path,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use derivative::*;
use linked_hash_set::LinkedHashSet;
use tokio::sync::{mpsc, RwLock};
#[allow(unused_imports, dead_code)]
use tracing::{debug, error, info, trace, warn};
use virtual_fs::{
    ArcBoxFile, ArcFile, AsyncWriteExt, CombineFile, DeviceFile, DuplexPipe, FileSystem, Pipe,
    PipeRx, PipeTx, RootFileSystemBuilder, VirtualFile,
};
#[cfg(feature = "sys")]
use wasmer::Engine;
use wasmer_wasix_types::{types::__WASI_STDIN_FILENO, wasi::Errno};

use super::{cconst::ConsoleConst, common::*, task::TaskJoinHandle};
use crate::{
    bin_factory::{spawn_exec, BinFactory, BinaryPackage},
    capabilities::Capabilities,
    os::task::{control_plane::WasiControlPlane, process::WasiProcess},
    runtime::resolver::PackageSpecifier,
    Runtime, SpawnError, VirtualTaskManagerExt, WasiEnv,
};

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Console {
    user_agent: Option<String>,
    boot_cmd: String,
    uses: LinkedHashSet<String>,
    is_mobile: bool,
    is_ssh: bool,
    whitelabel: bool,
    token: Option<String>,
    no_welcome: bool,
    prompt: String,
    env: HashMap<String, String>,
    runtime: Arc<dyn Runtime + Send + Sync + 'static>,
    stdin: ArcBoxFile,
    stdout: ArcBoxFile,
    stderr: ArcBoxFile,
    capabilities: Capabilities,
    memfs_memory_limiter: Option<virtual_fs::limiter::DynFsMemoryLimiter>,
}

impl Console {
    pub fn new(webc_boot_package: &str, runtime: Arc<dyn Runtime + Send + Sync + 'static>) -> Self {
        let prog = webc_boot_package
            .split_once(' ')
            .map(|a| a.1)
            .unwrap_or(webc_boot_package);

        let mut uses = LinkedHashSet::new();
        uses.insert(prog.to_string());
        Self {
            boot_cmd: webc_boot_package.to_string(),
            uses,
            is_mobile: false,
            is_ssh: false,
            user_agent: None,
            whitelabel: false,
            token: None,
            no_welcome: false,
            env: HashMap::new(),
            runtime,
            prompt: "wasmer.sh".to_string(),
            stdin: ArcBoxFile::new(Box::new(Pipe::channel().0)),
            stdout: ArcBoxFile::new(Box::new(Pipe::channel().0)),
            stderr: ArcBoxFile::new(Box::new(Pipe::channel().0)),
            capabilities: Default::default(),
            memfs_memory_limiter: None,
        }
    }

    pub fn with_prompt(mut self, prompt: String) -> Self {
        self.prompt = prompt;
        self
    }

    pub fn with_boot_cmd(mut self, cmd: String) -> Self {
        let prog = cmd.split_once(' ').map(|a| a.0).unwrap_or(cmd.as_str());
        self.uses.insert(prog.to_string());
        self.boot_cmd = cmd;
        self
    }

    pub fn with_uses(mut self, uses: Vec<String>) -> Self {
        self.uses = uses.into_iter().collect();
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_user_agent(mut self, user_agent: &str) -> Self {
        self.is_mobile = is_mobile(user_agent);
        self.is_ssh = is_ssh(user_agent);
        self.user_agent = Some(user_agent.to_string());
        self
    }

    pub fn with_no_welcome(mut self, no_welcome: bool) -> Self {
        self.no_welcome = no_welcome;
        self
    }

    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self
    }

    pub fn with_stdin(mut self, stdin: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.stdin = ArcBoxFile::new(stdin);
        self
    }

    pub fn with_stdout(mut self, stdout: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.stdout = ArcBoxFile::new(stdout);
        self
    }

    pub fn with_stderr(mut self, stderr: Box<dyn VirtualFile + Send + Sync + 'static>) -> Self {
        self.stderr = ArcBoxFile::new(stderr);
        self
    }

    pub fn with_mem_fs_memory_limiter(
        mut self,
        limiter: virtual_fs::limiter::DynFsMemoryLimiter,
    ) -> Self {
        self.memfs_memory_limiter = Some(limiter);
        self
    }

    pub fn run(&mut self) -> Result<(TaskJoinHandle, WasiProcess), SpawnError> {
        // Extract the program name from the arguments
        let empty_args: Vec<&[u8]> = Vec::new();
        let (webc, prog, args) = match self.boot_cmd.split_once(' ') {
            Some((webc, args)) => (
                webc,
                webc.split_once('/').map(|a| a.1).unwrap_or(webc),
                args.split(' ').map(|a| a.as_bytes()).collect::<Vec<_>>(),
            ),
            None => (
                self.boot_cmd.as_str(),
                self.boot_cmd
                    .split_once('/')
                    .map(|a| a.1)
                    .unwrap_or(self.boot_cmd.as_str()),
                empty_args,
            ),
        };
        let envs = self.env.clone();

        // Build a new store that will be passed to the threadimpo
        let store = self.runtime.new_store();

        let root_fs = RootFileSystemBuilder::new()
            .with_tty(Box::new(CombineFile::new(
                Box::new(self.stdout.clone()),
                Box::new(self.stdin.clone()),
            )))
            .build();

        let env_init = WasiEnv::builder(prog)
            .stdin(Box::new(self.stdin.clone()))
            .args(args.iter())
            .envs(envs.iter())
            .sandbox_fs(root_fs)
            .preopen_dir(Path::new("/"))
            .unwrap()
            .map_dir(".", "/")
            .unwrap()
            .stdout(Box::new(self.stdout.clone()))
            .stderr(Box::new(self.stderr.clone()))
            .runtime(self.runtime.clone())
            .capabilities(self.capabilities.clone())
            .build_init()
            // TODO: propagate better error
            .map_err(|_e| SpawnError::InternalError)?;

        // TODO: no unwrap!
        let env = WasiEnv::from_init(env_init).unwrap();

        if let Some(limiter) = &self.memfs_memory_limiter {
            match &env.state.fs.root_fs {
                crate::fs::WasiFsRoot::Sandbox(tmpfs) => {
                    tmpfs.set_memory_limiter(limiter.clone());
                }
                crate::fs::WasiFsRoot::Backing(_) => {
                    tracing::error!("tried to set a tmpfs memory limiter on a backing fs");
                    return Err(SpawnError::BadRequest);
                }
            }
        }

        // TODO: this should not happen here...
        // Display the welcome message
        let tasks = env.tasks().clone();
        if !self.whitelabel && !self.no_welcome {
            tasks.block_on(self.draw_welcome());
        }

        let webc_ident: PackageSpecifier = match webc.parse() {
            Ok(ident) => ident,
            Err(e) => {
                tracing::debug!(webc, error = &*e, "Unable to parse the WEBC identifier");
                return Err(SpawnError::BadRequest);
            }
        };

        let resolved_package =
            tasks.block_on(BinaryPackage::from_registry(&webc_ident, env.runtime()));

        let binary = match resolved_package {
            Ok(pkg) => pkg,
            Err(e) => {
                let mut stderr = self.stderr.clone();
                tasks.block_on(async {
                    let mut buffer = Vec::new();
                    writeln!(buffer, "Error: {e}").ok();
                    let mut source = e.source();
                    while let Some(s) = source {
                        writeln!(buffer, "  Caused by: {s}").ok();
                        source = s.source();
                    }

                    virtual_fs::AsyncWriteExt::write_all(&mut stderr, &buffer)
                        .await
                        .ok();
                });
                tracing::debug!("failed to get webc dependency - {}", webc);
                return Err(SpawnError::NotFound);
            }
        };

        let wasi_process = env.process.clone();

        // TODO: fetching dependencies should be moved to the builder!
        // TODO: the Console only makes sense in the context of SSH and the terminal.
        // We should make this just take a WasiBuilder and the console related configs
        // and not add so much custom logic in here.
        if let Err(err) = env.uses(self.uses.clone()) {
            let mut stderr = self.stderr.clone();
            tasks.block_on(async {
                virtual_fs::AsyncWriteExt::write_all(
                    &mut stderr,
                    format!("{}\r\n", err).as_bytes(),
                )
                .await
                .ok();
            });
            tracing::debug!("failed to load used dependency - {}", err);
            return Err(SpawnError::BadRequest);
        }

        // Build the config
        // Run the binary
        let process = tasks.block_on(spawn_exec(binary, prog, store, env, &self.runtime))?;

        // Return the process
        Ok((process, wasi_process))
    }

    pub async fn draw_welcome(&self) {
        let welcome = match (self.is_mobile, self.is_ssh) {
            (true, _) => ConsoleConst::WELCOME_MEDIUM,
            (_, true) => ConsoleConst::WELCOME_SMALL,
            (_, _) => ConsoleConst::WELCOME_LARGE,
        };
        let mut data = welcome
            .replace("\\x1B", "\x1B")
            .replace("\\r", "\r")
            .replace("\\n", "\n");
        data.insert_str(0, ConsoleConst::TERM_NO_WRAPAROUND);

        let mut stderr = self.stderr.clone();
        virtual_fs::AsyncWriteExt::write_all(&mut stderr, data.as_str().as_bytes())
            .await
            .ok();
    }
}
