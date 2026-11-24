use std::future::Future;
use std::io;

use zrun::io::{Handle, Poller, Reactor};
use zrun::{JoinHandle, Pool, PoolSpawner, Runtime, SpawnError, Timer};

const MAX_WORKERS: usize = 16;

pub(crate) enum Executor {
    Current(Runtime, Poller),
    Parallel {
        runtime: Pool,
        io: Handle,
        _reactor: Option<Reactor>,
    },
}

impl Executor {
    pub(crate) fn configured() -> io::Result<Self> {
        let workers = match std::env::var_os("ZRUN_WORKERS") {
            Some(value) => value
                .to_str()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|workers| (1..=MAX_WORKERS).contains(workers))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "ZRUN_WORKERS must be 1..16")
                })?,
            None => 1,
        };
        let driver = std::env::var_os("ZRUN_IO_DRIVER");
        let worker_io = match driver.as_deref().map(std::ffi::OsStr::as_encoded_bytes) {
            None | Some(b"thread") => false,
            Some(b"worker") => true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZRUN_IO_DRIVER must be thread or worker",
                ));
            }
        };
        if workers == 1 {
            return Self::current();
        }
        let (runtime, io, reactor) = if worker_io {
            let (runtime, io) = Pool::new_with_io(workers)?;
            (runtime, io, None)
        } else {
            let runtime = Pool::new(workers)?;
            let reactor = Reactor::new()?;
            (runtime, reactor.handle().clone(), Some(reactor))
        };
        Ok(Self::Parallel {
            runtime,
            io,
            _reactor: reactor,
        })
    }

    pub(crate) fn current() -> io::Result<Self> {
        Ok(Self::Current(Runtime::new(), Poller::new()?))
    }

    pub(crate) fn io(&self) -> &Handle {
        match self {
            Self::Current(_, poller) => poller.handle(),
            Self::Parallel { io, .. } => io,
        }
    }

    pub(crate) fn timer(&self) -> Timer {
        match self {
            Self::Current(runtime, _) => runtime.timer(),
            Self::Parallel { runtime, .. } => runtime.timer(),
        }
    }

    pub(crate) fn spawner(&self) -> Spawner {
        match self {
            Self::Current(runtime, _) => Spawner::Current(runtime.spawner()),
            Self::Parallel { runtime, .. } => Spawner::Parallel(runtime.spawner()),
        }
    }

    pub(crate) fn block_on<F: Future>(&mut self, future: F) -> io::Result<F::Output> {
        match self {
            Self::Current(runtime, poller) => runtime.block_on_with_io(poller, future),
            Self::Parallel { runtime, .. } => Ok(runtime.block_on(future)),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        match self {
            Self::Current(runtime, _) => runtime.shutdown(),
            Self::Parallel { runtime, .. } => runtime.shutdown(),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Spawner {
    Current(zrun::Spawner),
    Parallel(PoolSpawner),
}

impl Spawner {
    pub(crate) fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, SpawnError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            Self::Current(spawner) => spawner.spawn(future),
            Self::Parallel(spawner) => spawner.spawn(future),
        }
    }

    pub(crate) fn spawn_batch<F>(
        &self,
        futures: impl IntoIterator<Item = F>,
    ) -> Vec<Result<JoinHandle<F::Output>, SpawnError>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        match self {
            Self::Current(spawner) => futures
                .into_iter()
                .map(|future| spawner.spawn(future))
                .collect(),
            Self::Parallel(spawner) => spawner.spawn_batch(futures),
        }
    }
}
