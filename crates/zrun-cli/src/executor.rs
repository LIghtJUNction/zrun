use std::future::Future;
use std::io;

use zrun::{JoinHandle, Pool, PoolSpawner, Runtime, SpawnError, Timer};

const MAX_WORKERS: usize = 16;

pub(crate) enum Executor {
    Current(Runtime),
    Parallel(Pool),
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
        if workers == 1 {
            Ok(Self::current())
        } else {
            Pool::new(workers).map(Self::Parallel)
        }
    }

    pub(crate) fn current() -> Self {
        Self::Current(Runtime::new())
    }

    pub(crate) fn timer(&self) -> Timer {
        match self {
            Self::Current(runtime) => runtime.timer(),
            Self::Parallel(runtime) => runtime.timer(),
        }
    }

    pub(crate) fn spawner(&self) -> Spawner {
        match self {
            Self::Current(runtime) => Spawner::Current(runtime.spawner()),
            Self::Parallel(runtime) => Spawner::Parallel(runtime.spawner()),
        }
    }

    pub(crate) fn block_on<F: Future>(&mut self, future: F) -> F::Output {
        match self {
            Self::Current(runtime) => runtime.block_on(future),
            Self::Parallel(runtime) => runtime.block_on(future),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        match self {
            Self::Current(runtime) => runtime.shutdown(),
            Self::Parallel(runtime) => runtime.shutdown(),
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
}
