use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};

use zrun::sync::{self, Sender};
use zrun_plugin_api::{ALLOW, AdmissionApi, DENY};

use crate::execution::Request;

const QUEUE: usize = 128;

struct Check {
    shell: PathBuf,
    cwd: PathBuf,
    script: Arc<str>,
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
                let mut admission = match AdmissionApi::create(&library) {
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
                    let result = admission.command(
                        check
                            .shell
                            .to_str()
                            .expect("shell path was validated before admission"),
                        check
                            .cwd
                            .to_str()
                            .expect("working directory was validated before admission"),
                        &check.script,
                        check.timeout_ms,
                    );
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
