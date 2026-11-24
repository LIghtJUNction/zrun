use std::collections::{HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zrun::Timer;
use zrun::io::Handle;
use zrun::sync::{self, Receiver, Sender};

use crate::executor::{Executor, Spawner};
use crate::output::{Budget, Reservation};

use crate::plugin::Plugin;
use crate::policy::Gate;
use crate::process;

const MAX_COMMANDS: usize = 4_096;
const MAX_SCRIPT: usize = 65_536;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_OUTPUT: usize = 65_536;
const PARALLEL: usize = 32;
const BATCH_WINDOW: usize = 128;
const MAX_WAITERS: usize = 4_096;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 1_024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENCODED_OUTPUT: usize = 32 * 1024 * 1024;
const MAX_RAW_OUTPUT: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reuse {
    pub scope: String,
    pub input_revision: String,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub script: Arc<str>,
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    pub reuse: Option<Reuse>,
}

impl Request {
    pub(crate) fn new(script: String) -> Self {
        Self {
            script: script.into(),
            cwd: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            reuse: None,
        }
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.script.len() > MAX_SCRIPT || self.script.as_bytes().contains(&0) {
            return Err(invalid("invalid script length or bytes"));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(invalid("timeout_ms must be between 1 and 3600000"));
        }
        if let Some(reuse) = &self.reuse
            && (reuse.scope.is_empty()
                || reuse.scope.len() > 128
                || reuse.input_revision.is_empty()
                || reuse.input_revision.len() > 256
                || reuse.ttl_ms > MAX_TIMEOUT_MS)
        {
            return Err(invalid("invalid cache scope, revision or TTL"));
        }
        Ok(())
    }

    fn size(&self) -> usize {
        self.script
            .len()
            .saturating_add(self.cwd.as_ref().map_or(0, |p| p.as_os_str().len()))
            .saturating_add(
                self.reuse
                    .as_ref()
                    .map_or(0, |reuse| reuse.scope.len() + reuse.input_revision.len()),
            )
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ResultRecord {
    pub exit_code: i32,
    #[serde(with = "crate::bytes")]
    pub stdout: Vec<u8>,
    #[serde(with = "crate::bytes")]
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub error: Option<String>,
    #[serde(skip)]
    pub(crate) encoding: Encoding,
}

#[derive(Debug, Default)]
pub(crate) struct Encoding {
    prepared: Mutex<Weak<EncodedResult>>,
    size: OnceLock<EncodedSize>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EncodedSize {
    json: usize,
    json_string: usize,
}

impl EncodedSize {
    pub(crate) fn total(self) -> usize {
        self.json + self.json_string
    }
}

impl io::Write for EncodedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.json += bytes.len();
        // Compact JSON already escapes control characters. Quoting the whole
        // document adds one slash to each existing slash or quotation mark.
        self.json_string += bytes.len()
            + bytes
                .iter()
                .filter(|&&byte| matches!(byte, b'\\' | b'"'))
                .count();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct EncodedResult {
    json: Box<[u8]>,
    json_string: Box<[u8]>,
    reservation: Reservation,
}

impl EncodedResult {
    pub(crate) fn json(&self) -> &[u8] {
        &self.json
    }

    pub(crate) fn json_string_contents(&self) -> &[u8] {
        &self.json_string[1..self.json_string.len() - 1]
    }
}

impl ResultRecord {
    fn error(error: io::Error) -> Self {
        Self {
            exit_code: 125,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            error: Some(error.to_string()),
            encoding: Encoding::default(),
        }
    }

    pub(crate) fn capture_capacity(&self) -> usize {
        self.stdout.capacity() + self.stderr.capacity()
    }

    pub(crate) fn prepared_encoding(&self) -> Option<Arc<EncodedResult>> {
        self.encoding
            .prepared
            .lock()
            .expect("result encoding poisoned")
            .upgrade()
    }

    pub(crate) fn encoded_size(&self) -> serde_json::Result<EncodedSize> {
        if let Some(size) = self.encoding.size.get() {
            return Ok(*size);
        }
        let mut size = EncodedSize {
            json: 0,
            json_string: 2,
        };
        serde_json::to_writer(&mut size, self)?;
        let _ = self.encoding.size.set(size);
        Ok(size)
    }

    pub(crate) fn encode(
        &self,
        size: EncodedSize,
        reservation: Reservation,
    ) -> serde_json::Result<Arc<EncodedResult>> {
        let mut previous = self
            .encoding
            .prepared
            .lock()
            .expect("result encoding poisoned");
        if let Some(encoded) = previous.upgrade() {
            return Ok(encoded);
        }
        let mut json = Vec::with_capacity(size.json);
        serde_json::to_writer(&mut json, self)?;
        let mut json_string = Vec::with_capacity(size.json_string);
        let text = std::str::from_utf8(&json).expect("compact JSON is UTF-8");
        serde_json::to_writer(&mut json_string, text)?;
        if (json.len(), json_string.len()) != (size.json, size.json_string) {
            return Err(serde_json::Error::io(io::Error::other(
                "encoded result size changed",
            )));
        }
        let encoded = Arc::new(EncodedResult {
            json: json.into_boxed_slice(),
            json_string: json_string.into_boxed_slice(),
            reservation,
        });
        // Idle cached records retain only raw output. Concurrent sends share
        // the encoded bytes while any of them still needs the representation.
        *previous = Arc::downgrade(&encoded);
        drop(previous);
        encoded.reservation.published();
        Ok(encoded)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Executed,
    Coalesced,
    Cached,
    Rejected,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Delivery {
    pub disposition: Disposition,
    pub result: Arc<ResultRecord>,
}

impl Delivery {
    pub(crate) fn rejected(error: io::Error) -> Self {
        Self {
            disposition: Disposition::Rejected,
            result: Arc::new(ResultRecord::error(error)),
        }
    }
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct Key {
    script: Arc<str>,
    cwd: PathBuf,
    timeout_ms: u64,
    reuse: Reuse,
}

struct Entry {
    result: Arc<ResultRecord>,
    expires: Instant,
    size: usize,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Arc<Key>, Entry>,
    order: VecDeque<Arc<Key>>,
    bytes: usize,
}

impl Cache {
    fn get(&self, key: &Key) -> Option<Arc<ResultRecord>> {
        let entry = self.entries.get(key)?;
        (entry.expires > Instant::now()).then(|| entry.result.clone())
    }

    fn insert(&mut self, key: Arc<Key>, result: Arc<ResultRecord>, command_bytes: usize) {
        if key.reuse.ttl_ms == 0
            || result.exit_code != 0
            || result.truncated
            || result.error.is_some()
        {
            return;
        }
        // Account allocated buffer capacity and bound metadata independently by entry count.
        let size = command_bytes + result.capture_capacity();
        if size > MAX_CACHE_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.size;
            self.order.retain(|entry| **entry != *key);
        }
        while self.entries.len() == MAX_CACHE_ENTRIES || self.bytes + size > MAX_CACHE_BYTES {
            let oldest = self
                .order
                .pop_front()
                .expect("nonempty cache has an eviction key");
            let entry = self
                .entries
                .remove(&oldest)
                .expect("the eviction key is present");
            self.bytes -= entry.size;
        }
        let expires = Instant::now() + Duration::from_millis(key.reuse.ttl_ms);
        self.bytes += size;
        self.order.push_back(key.clone());
        self.entries.insert(
            key,
            Entry {
                result,
                expires,
                size,
            },
        );
    }
}

struct Subscriber {
    sender: Sender<Delivery>,
    disposition: Disposition,
}

enum Completion {
    Single(Subscriber),
    Shared(Arc<Key>),
}

struct Job {
    request: Request,
    size: usize,
    completion: Completion,
}

struct State {
    queue: VecDeque<Job>,
    sleeping: Vec<Option<Waker>>,
    in_flight: HashMap<Arc<Key>, Vec<Subscriber>>,
    cache: Cache,
    pending_bytes: usize,
    waiters: usize,
}

struct Shared {
    state: Mutex<State>,
    gate: Mutex<Gate>,
    plugin: Option<Plugin>,
    shell: PathBuf,
    directory: PathBuf,
    output_budget: Arc<Budget>,
    raw_output_budget: Arc<Budget>,
    io: Handle,
    timer: Timer,
}

pub(crate) struct Broker {
    shared: Arc<Shared>,
}

impl Broker {
    pub(crate) fn new(
        shell: PathBuf,
        io: &Handle,
        timer: Timer,
        spawner: &Spawner,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                sleeping: vec![None; PARALLEL],
                in_flight: HashMap::new(),
                cache: Cache::default(),
                pending_bytes: 0,
                waiters: 0,
            }),
            gate: Mutex::new(Gate::from_env()?),
            plugin: Plugin::from_env()?,
            shell,
            directory: std::env::current_dir()?,
            output_budget: Budget::new(MAX_ENCODED_OUTPUT),
            raw_output_budget: Budget::new(MAX_RAW_OUTPUT),
            io: io.clone(),
            timer,
        });
        let mut workers = Vec::with_capacity(PARALLEL);
        for slot in 0..PARALLEL {
            match spawner.spawn(worker(shared.clone(), slot)) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    for worker in &workers {
                        worker.abort();
                    }
                    return Err(io::Error::other(error));
                }
            }
        }
        Ok(Self { shared })
    }

    pub(crate) fn output_budget(&self) -> &Arc<Budget> {
        &self.shared.output_budget
    }

    pub(crate) async fn reserve_capture(&self, commands: usize) -> Reservation {
        self.shared
            .raw_output_budget
            .reserve(commands * 2 * MAX_OUTPUT)
            .await
    }

    pub(crate) async fn submit(&self, mut request: Request) -> io::Result<Receiver<Delivery>> {
        request.validate()?;
        let cwd = resolve_directory(&self.shared.directory, request.cwd.as_deref())?;
        request.cwd = Some(cwd.clone());
        self.shared
            .gate
            .lock()
            .expect("command policy poisoned")
            .authorize(&self.shared.shell, &request)?;
        if let Some(plugin) = &self.shared.plugin {
            plugin.authorize(&self.shared.shell, &request).await?;
        }
        let key = request.reuse.as_ref().map(|reuse| {
            Arc::new(Key {
                script: request.script.clone(),
                cwd,
                timeout_ms: request.timeout_ms,
                reuse: reuse.clone(),
            })
        });
        let (sender, receiver) = sync::channel();
        let mut state = self.shared.state.lock().expect("broker state poisoned");
        if let Some(delivery) = key.as_ref().and_then(|key| state.cache.get(key)) {
            drop(state);
            let _ = sender.send(Delivery {
                disposition: Disposition::Cached,
                result: delivery,
            });
            return Ok(receiver);
        }
        if state.waiters == MAX_WAITERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "command queue is full",
            ));
        }
        if let Some(key) = &key
            && let Some(subscribers) = state.in_flight.get_mut(key)
        {
            subscribers.push(Subscriber {
                sender,
                disposition: Disposition::Coalesced,
            });
            state.waiters += 1;
            return Ok(receiver);
        }
        let size = request.size();
        if size > MAX_PENDING_BYTES - state.pending_bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "command queue byte limit reached",
            ));
        }
        let subscriber = Subscriber {
            sender,
            disposition: Disposition::Executed,
        };
        let completion = if let Some(key) = key {
            state.in_flight.insert(key.clone(), vec![subscriber]);
            Completion::Shared(key)
        } else {
            Completion::Single(subscriber)
        };
        state.waiters += 1;
        state.pending_bytes += size;
        state.queue.push_back(Job {
            request,
            completion,
            size,
        });
        let idle = state.sleeping.iter_mut().find_map(Option::take);
        drop(state);
        if let Some(waker) = idle {
            waker.wake();
        }
        Ok(receiver)
    }
}

fn resolve_directory(base: &Path, cwd: Option<&Path>) -> io::Result<PathBuf> {
    match cwd {
        Some(cwd) => base.join(cwd).canonicalize(),
        None => Ok(base.to_path_buf()),
    }
}

async fn worker(shared: Arc<Shared>, slot: usize) {
    loop {
        let job = poll_fn(|cx| {
            let incoming = cx.waker().clone();
            let mut state = shared.state.lock().expect("broker state poisoned");
            let job = state.queue.pop_front();
            let idle = if job.is_none() {
                state.sleeping[slot].replace(incoming)
            } else {
                state.sleeping[slot].take()
            };
            drop(state);
            drop(idle);
            job.map_or(Poll::Pending, Poll::Ready)
        })
        .await;
        let result = Arc::new(
            process::execute(
                &shared.shell,
                job.request,
                &shared.io,
                &shared.timer,
                MAX_OUTPUT,
            )
            .await
            .unwrap_or_else(ResultRecord::error),
        );
        let subscribers = {
            let mut state = shared.state.lock().expect("broker state poisoned");
            let subscribers = match job.completion {
                Completion::Single(subscriber) => vec![subscriber],
                Completion::Shared(key) => {
                    let subscribers = state
                        .in_flight
                        .remove(&key)
                        .expect("shared job has subscribers");
                    state.cache.insert(key, result.clone(), job.size);
                    subscribers
                }
            };
            state.pending_bytes -= job.size;
            state.waiters -= subscribers.len();
            subscribers
        };
        for Subscriber {
            sender,
            disposition,
        } in subscribers
        {
            let _ = sender.send(Delivery {
                disposition,
                result: result.clone(),
            });
        }
        zrun::yield_now().await;
    }
}

pub(crate) struct Batch {
    runtime: Executor,
    broker: Broker,
}

impl Batch {
    pub(crate) const MAX_INPUT: usize = 2 * 1024 * 1024;

    pub(crate) fn new(shell: PathBuf) -> io::Result<Self> {
        let runtime = Executor::configured()?;
        let broker = Broker::new(shell, runtime.io(), runtime.timer(), &runtime.spawner())?;
        Ok(Self { runtime, broker })
    }

    pub(crate) fn drive<T>(
        mut self,
        program: impl AsyncFnOnce(&Broker) -> io::Result<T>,
    ) -> io::Result<T> {
        self.runtime.block_on(program(&self.broker))?
    }

    pub(crate) fn execute(
        mut self,
        requests: Vec<Request>,
        mut record: impl FnMut(usize, Delivery) -> io::Result<()>,
    ) -> io::Result<()> {
        if requests.len() > MAX_COMMANDS {
            return Err(invalid("batch exceeds the command count limit"));
        }
        for request in &requests {
            request.validate()?;
        }
        let mut requests = requests.into_iter().enumerate();
        self.runtime.block_on(async {
            let mut running = Vec::with_capacity(BATCH_WINDOW);
            loop {
                while running.len() < BATCH_WINDOW {
                    let Some((index, request)) = requests.next() else {
                        break;
                    };
                    match self.broker.submit(request).await {
                        Ok(ticket) => running.push((index, ticket)),
                        Err(error) => record(index, Delivery::rejected(error))?,
                    }
                }
                if running.is_empty() {
                    return Ok(());
                }
                let (slot, delivery) = poll_fn(|cx| {
                    for (slot, (_, ticket)) in running.iter_mut().enumerate() {
                        if let Poll::Ready(delivery) = Pin::new(ticket).poll(cx) {
                            return Poll::Ready((slot, delivery));
                        }
                    }
                    Poll::Pending
                })
                .await;
                let (index, _) = running.swap_remove(slot);
                record(index, delivery.map_err(io::Error::other)?)?;
            }
        })?
    }
}
