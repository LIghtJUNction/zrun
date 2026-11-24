use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};

use zrun::sync::{self, Sender};
use zrun_plugin_api::{ALLOW, AdmissionApi, AdmissionV2Api, DENY};

use crate::execution::Request;

const QUEUE: usize = 128;

struct Check {
    shell: PathBuf,
    cwd: PathBuf,
    script: Arc<str>,
    args: Arc<Vec<String>>,
    timeout_ms: u64,
    reply: Sender<io::Result<()>>,
}

pub(crate) struct Plugin(SyncSender<Check>);

impl Plugin {
    pub(crate) fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("ZRUN_PLUGIN") else {
            return Ok(None);
        };
        let path = PathBuf::from(path).canonicalize()?;
        let (send, receive) = mpsc::sync_channel::<Check>(QUEUE);
        let (startup, ready) = mpsc::sync_channel(0);
        std::thread::Builder::new()
            .name("zrun-plugin".into())
            .spawn(move || {
                // SAFETY: The operator explicitly selects trusted native code at
                // startup. Pinned loading keeps the library mapped for the process.
                let library = match unsafe {
                    zplugin::Plugin::load_path(&path, zplugin::UnloadPolicy::Pinned)
                } {
                    Ok(library) => library,
                    Err(error) => {
                        let _ = startup.send(Err(io::Error::other(error)));
                        return;
                    }
                };
                let admission = match AdmissionV2Api::create(&library) {
                    Ok(admission) => Ok((None, Some(admission))),
                    Err(zplugin::Error::InterfaceVersion {
                        expected: 2,
                        actual: 1,
                    }) => AdmissionApi::create(&library).map(|admission| (Some(admission), None)),
                    Err(error) => Err(error),
                };
                let (mut legacy, mut current) = match admission {
                    Ok(admission) => admission,
                    Err(error) => {
                        let _ = startup.send(Err(io::Error::other(error)));
                        return;
                    }
                };
                if startup.send(Ok(())).is_err() {
                    return;
                }
                for check in receive {
                    let shell = check
                        .shell
                        .to_str()
                        .expect("shell path was validated before admission");
                    let cwd = check
                        .cwd
                        .to_str()
                        .expect("working directory was validated before admission");
                    let result = if let Some(admission) = current.as_mut() {
                        let mut args = Vec::new();
                        for arg in check.args.iter() {
                            args.extend_from_slice(arg.as_bytes());
                            args.push(0);
                        }
                        admission.command(shell, cwd, &check.script, check.timeout_ms, &args)
                    } else if check.args.is_empty() {
                        legacy
                            .as_mut()
                            .expect("legacy admission was loaded")
                            .command(shell, cwd, &check.script, check.timeout_ms)
                    } else {
                        let _ = check.reply.send(Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "the configured admission plugin cannot inspect positional arguments",
                        )));
                        continue;
                    };
                    let decision = match result {
                        Ok(ALLOW) => Ok(()),
                        Ok(DENY) => Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "zplugin denied the request",
                        )),
                        Ok(_) => Err(io::Error::other(
                            "native admission plugin returned an unknown decision",
                        )),
                        Err(error) => Err(io::Error::other(error)),
                    };
                    let _ = check.reply.send(decision);
                }
            })?;
        ready.recv().map_err(io::Error::other)??;
        Ok(Some(Self(send)))
    }

    pub(crate) async fn authorize(&self, shell: &Path, request: &Request) -> io::Result<()> {
        let (reply, response) = sync::channel();
        let check = Check {
            shell: shell.to_path_buf(),
            cwd: request
                .cwd
                .clone()
                .expect("working directory was resolved before admission"),
            script: request.script.clone(),
            args: request.args.clone(),
            timeout_ms: request.timeout_ms,
            reply,
        };
        self.0.try_send(check).map_err(|error| match error {
            TrySendError::Full(_) => {
                io::Error::new(io::ErrorKind::WouldBlock, "native admission queue is full")
            }
            TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "native admission plugin closed")
            }
        })?;
        response.await.map_err(io::Error::other)?
    }
}
