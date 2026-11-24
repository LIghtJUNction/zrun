//! One fresh process produces one sample. Tokio exists only in this comparator.
use serde_json::{Value, json};
use std::{
    future::{Future, poll_fn},
    io::Write,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

#[derive(Debug)]
struct Config {
    engine: String,
    scenario: String,
    tasks: usize,
    workers: usize,
    yields: usize,
    latency_samples: usize,
    hold_ms: u64,
    duration_secs: Option<u64>,
    sample_interval_ms: u64,
}

impl Config {
    fn parse() -> Result<Self, String> {
        let mut c = Self {
            engine: "zrun".into(),
            scenario: "spawn_join".into(),
            tasks: 1000,
            workers: 1,
            yields: 16,
            latency_samples: 10000,
            hold_ms: 20,
            duration_secs: None,
            sample_interval_ms: 1000,
        };
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            if key == "--help" {
                println!(
                    "zrun-bench --engine zrun|tokio --scenario spawn_join|batch_join|yield|wake_latency|external_wake_latency|batch_external_wake_latency|pending_memory|idle|soak --tasks 1..1000000 --workers 1|2|4|8|16 [--yields 16] [--latency-samples 10000] [--hold-ms 20] [--duration-secs N --sample-interval-ms 1000]"
                );
                std::process::exit(0);
            }
            let v = args
                .next()
                .ok_or_else(|| format!("missing value for {key}"))?;
            match key.as_str() {
                "--engine" => c.engine = v,
                "--scenario" => c.scenario = v,
                "--tasks" => c.tasks = v.parse().map_err(|_| "invalid tasks")?,
                "--workers" => c.workers = v.parse().map_err(|_| "invalid workers")?,
                "--yields" => c.yields = v.parse().map_err(|_| "invalid yields")?,
                "--latency-samples" => {
                    c.latency_samples = v.parse().map_err(|_| "invalid latency samples")?
                }
                "--hold-ms" => c.hold_ms = v.parse().map_err(|_| "invalid hold-ms")?,
                "--duration-secs" => {
                    c.duration_secs = Some(v.parse().map_err(|_| "invalid duration-secs")?)
                }
                "--sample-interval-ms" => {
                    c.sample_interval_ms = v.parse().map_err(|_| "invalid sample interval")?
                }
                _ => return Err(format!("unknown argument {key}")),
            }
        }
        if !["zrun", "tokio"].contains(&c.engine.as_str()) {
            return Err("unknown engine".into());
        }
        if ![
            "spawn_join",
            "batch_join",
            "yield",
            "wake_latency",
            "external_wake_latency",
            "batch_external_wake_latency",
            "pending_memory",
            "idle",
            "soak",
        ]
        .contains(&c.scenario.as_str())
        {
            return Err("unknown scenario".into());
        }
        if !(1..=1_000_000).contains(&c.tasks)
            || ![1, 2, 4, 8, 16].contains(&c.workers)
            || c.yields > 10_000
            || (c.yields == 0 && c.scenario != "batch_join")
            || !(1..=100_000).contains(&c.latency_samples)
            || c.hold_ms > 10_000
            || !(1..=60_000).contains(&c.sample_interval_ms)
        {
            return Err("configuration outside documented bounds".into());
        }
        if c.scenario == "batch_external_wake_latency" && c.workers == 1 {
            return Err("batch_external_wake_latency requires at least two workers".into());
        }
        if c.scenario == "soak" && !c.duration_secs.is_some_and(|s| (1..=604800).contains(&s)) {
            return Err("soak requires explicit --duration-secs in 1..604800".into());
        }
        Ok(c)
    }

    fn record(&self, status: &str) -> Value {
        json!({"schema_version": 1, "engine": self.engine, "scenario": self.scenario,
            "workers": self.workers, "tasks": self.tasks, "status": status,
            "config": {"yields": self.yields, "latency_samples": self.latency_samples,
                "hold_ms": self.hold_ms, "duration_secs": self.duration_secs,
                "sample_interval_ms": self.sample_interval_ms, "timer_enabled": true,
                "yield_includes_submission": self.workers > 1},
            "build": {"rustc": env!("ZRUN_BENCH_RUSTC"), "profile": env!("ZRUN_BENCH_PROFILE"),
                "target": env!("ZRUN_BENCH_TARGET"), "opt_level": env!("ZRUN_BENCH_OPT_LEVEL"),
                "encoded_rustflags": env!("ZRUN_BENCH_CARGO_ENCODED_RUSTFLAGS"),
                "tokio": "1.54.1", "comparator": env!("CARGO_PKG_VERSION")}})
    }
}

trait Abort {
    fn abort(&self);
}

struct ZrunJoin<T>(zrun::JoinHandle<T>);
impl<T> Abort for ZrunJoin<T> {
    fn abort(&self) {
        self.0.abort();
    }
}
impl<T> Future for ZrunJoin<T> {
    type Output = Result<T, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(cx).map(|v| {
            v.map_err(|e| {
                if e.is_cancelled() {
                    "cancelled".into()
                } else {
                    format!("non-cancellation join failure: {e}")
                }
            })
        })
    }
}

struct TokioJoin<T>(tokio::task::JoinHandle<T>);
impl<T> Abort for TokioJoin<T> {
    fn abort(&self) {
        self.0.abort();
    }
}
impl<T> Future for TokioJoin<T> {
    type Output = Result<T, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(cx).map(|v| {
            v.map_err(|e| {
                if e.is_cancelled() {
                    "cancelled".into()
                } else {
                    format!("non-cancellation join failure: {e}")
                }
            })
        })
    }
}

trait Engine: Sized {
    type Handle<T>: Future<Output = Result<T, String>> + Unpin + Abort
    where
        T: Send + 'static;
    fn new(config: &Config) -> Result<Self, String>;
    fn spawn<F>(&self, future: F) -> Result<Self::Handle<F::Output>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;
    fn spawn_batch<F>(
        &self,
        futures: impl IntoIterator<Item = F>,
    ) -> Result<Vec<Self::Handle<F::Output>>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        futures
            .into_iter()
            .map(|future| self.spawn(future))
            .collect()
    }
    fn block_on<F: Future>(&mut self, future: F) -> F::Output;
    fn alive(&self) -> usize;
}

impl Engine for zrun::Runtime {
    type Handle<T>
        = ZrunJoin<T>
    where
        T: Send + 'static;
    fn new(config: &Config) -> Result<Self, String> {
        Self::with_max_tasks(config.tasks).map_err(|e| e.to_string())
    }
    fn spawn<F>(&self, f: F) -> Result<Self::Handle<F::Output>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn(f).map(ZrunJoin).map_err(|e| e.to_string())
    }
    fn block_on<F: Future>(&mut self, f: F) -> F::Output {
        self.block_on(f)
    }
    fn alive(&self) -> usize {
        self.stats().active_tasks
    }
}

impl Engine for zrun::Pool {
    type Handle<T>
        = ZrunJoin<T>
    where
        T: Send + 'static;
    fn new(config: &Config) -> Result<Self, String> {
        Self::with_max_tasks(config.workers, config.tasks).map_err(|e| e.to_string())
    }
    fn spawn<F>(&self, f: F) -> Result<Self::Handle<F::Output>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn(f).map(ZrunJoin).map_err(|e| e.to_string())
    }
    fn spawn_batch<F>(
        &self,
        futures: impl IntoIterator<Item = F>,
    ) -> Result<Vec<Self::Handle<F::Output>>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_batch(futures)
            .into_iter()
            .map(|result| result.map(ZrunJoin).map_err(|error| error.to_string()))
            .collect()
    }
    fn block_on<F: Future>(&mut self, f: F) -> F::Output {
        self.block_on(f)
    }
    fn alive(&self) -> usize {
        self.stats().active_tasks
    }
}

impl Engine for tokio::runtime::Runtime {
    type Handle<T>
        = TokioJoin<T>
    where
        T: Send + 'static;
    fn new(config: &Config) -> Result<Self, String> {
        let mut builder = if config.workers == 1 && config.scenario != "batch_join" {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(config.workers);
            builder
        };
        builder.enable_time().build().map_err(|e| e.to_string())
    }
    fn spawn<F>(&self, f: F) -> Result<Self::Handle<F::Output>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(TokioJoin(self.spawn(f)))
    }
    fn block_on<F: Future>(&mut self, f: F) -> F::Output {
        tokio::runtime::Runtime::block_on(self, f)
    }
    fn alive(&self) -> usize {
        self.metrics().num_alive_tasks()
    }
}

struct YieldWork {
    remaining: usize,
    index: usize,
}
impl Future for YieldWork {
    type Output = usize;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
        if self.remaining == 0 {
            return Poll::Ready(self.index);
        }
        self.remaining -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

struct WakeWork {
    index: usize,
    sampled: bool,
    armed: bool,
    start: Option<Instant>,
}
impl Future for WakeWork {
    type Output = (usize, Option<u64>);
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.armed {
            return Poll::Ready((
                self.index,
                self.start.map(|t| t.elapsed().as_nanos() as u64),
            ));
        }
        self.armed = true;
        if self.sampled {
            self.start = Some(Instant::now());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

type WakeSample = (usize, Option<u64>);

struct ExternalWake {
    index: usize,
    sampled: bool,
    signal: Arc<OnceLock<Option<Instant>>>,
    register: Option<mpsc::Sender<WakeRegistration>>,
}

struct WakeRegistration {
    sampled: bool,
    signal: Arc<OnceLock<Option<Instant>>>,
    waker: Waker,
}

impl Future for ExternalWake {
    type Output = WakeSample;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(start) = self.signal.get() {
            return Poll::Ready((self.index, start.map(|t| t.elapsed().as_nanos() as u64)));
        }
        if let Some(register) = self.register.take() {
            register
                .send(WakeRegistration {
                    sampled: self.sampled,
                    signal: self.signal.clone(),
                    waker: cx.waker().clone(),
                })
                .expect("the benchmark producer receives every registration");
        }
        Poll::Pending
    }
}

fn external_wake_samples<E: Engine>(
    runtime: &mut E,
    tasks: usize,
    count: usize,
) -> Result<Vec<u64>, String> {
    let (register, ready) = mpsc::channel::<WakeRegistration>();
    let producer = std::thread::Builder::new()
        .name("external-wake".into())
        .spawn(move || {
            let mut registered = Vec::with_capacity(tasks);
            for _ in 0..tasks {
                registered.push(ready.recv().map_err(|error| error.to_string())?);
            }
            for entry in registered {
                let start = entry.sampled.then(Instant::now);
                entry.signal.set(start).expect("one external wake per task");
                entry.waker.wake();
            }
            Ok::<_, String>(())
        })
        .map_err(|error| error.to_string())?;
    let mut handles = Vec::with_capacity(tasks);
    for index in 0..tasks {
        handles.push(runtime.spawn(ExternalWake {
            index,
            sampled: index * count / tasks != (index + 1) * count / tasks,
            signal: Arc::new(OnceLock::new()),
            register: Some(register.clone()),
        })?);
    }
    drop(register);
    let samples = join_wake_samples(runtime, handles, count);
    producer
        .join()
        .map_err(|_| "external wake producer panicked")??;
    samples
}

fn batch_external_wake_samples<E: Engine>(
    runtime: &mut E,
    tasks: usize,
    count: usize,
) -> Result<(Vec<u64>, usize), String> {
    let (register, ready) = mpsc::channel::<WakeRegistration>();
    let submitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let producer_submitted = submitted.clone();
    let producer = std::thread::Builder::new()
        .name("external-wake".into())
        .spawn(move || {
            let mut overlapping = 0;
            for _ in 0..tasks {
                let entry = ready.recv().map_err(|error| error.to_string())?;
                let start = entry.sampled.then(Instant::now);
                entry.signal.set(start).expect("one external wake per task");
                overlapping += usize::from(!producer_submitted.load(Ordering::Acquire));
                entry.waker.wake();
            }
            Ok::<_, String>(overlapping)
        })
        .map_err(|error| error.to_string())?;
    let handles = runtime.spawn_batch((0..tasks).map(|index| ExternalWake {
        index,
        sampled: index * count / tasks != (index + 1) * count / tasks,
        signal: Arc::new(OnceLock::new()),
        register: Some(register.clone()),
    }));
    submitted.store(true, Ordering::Release);
    drop(register);
    let samples = join_wake_samples(runtime, handles?, count);
    let overlapping = producer
        .join()
        .map_err(|_| "external wake producer panicked")??;
    Ok((samples?, overlapping))
}

fn join_wake_samples<E: Engine>(
    runtime: &mut E,
    handles: Vec<E::Handle<WakeSample>>,
    count: usize,
) -> Result<Vec<u64>, String> {
    let samples = runtime.block_on(async {
        let mut samples = Vec::with_capacity(count);
        for (expected, handle) in handles.into_iter().enumerate() {
            let (index, latency) = handle.await?;
            if index != expected {
                return Err("wake task identity mismatch".to_string());
            }
            if let Some(latency) = latency {
                samples.push(latency);
            }
        }
        Ok(samples)
    })?;
    if samples.len() != count {
        return Err("missing latency samples".into());
    }
    Ok(samples)
}

struct PendingWork {
    entered: bool,
    count: Arc<AtomicUsize>,
}
impl Future for PendingWork {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if !self.entered {
            self.entered = true;
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        Poll::Pending
    }
}

fn rss() -> Value {
    #[cfg(target_os = "linux")]
    let (source, value) = (
        "proc_status_vmrss",
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("VmRSS:")
                        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                        .and_then(|v| v.checked_mul(1024))
                })
            }),
    );
    #[cfg(target_os = "macos")]
    let (source, value) = (
        "ps_rss",
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .and_then(|v| v.checked_mul(1024)),
    );
    #[cfg(target_os = "windows")]
    let (source, value) = (
        "powershell_working_set",
        std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-Process -Id {}).WorkingSet64", std::process::id()),
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u64>()
                    .ok()
            }),
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let (source, value): (&str, Option<u64>) = ("unsupported", None);
    json!({"bytes": value, "source": source, "available": value.is_some()})
}

fn rss_delta(after: &Value, before: &Value) -> Option<i64> {
    Some(
        i64::try_from(after["bytes"].as_u64()?).ok()?
            - i64::try_from(before["bytes"].as_u64()?).ok()?,
    )
}

fn percentile(values: &mut [u64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let p = (values.len() - 1) as f64 * q;
    let a = p.floor() as usize;
    let b = p.ceil() as usize;
    Some(values[a] as f64 + (values[b] as f64 - values[a] as f64) * (p - a as f64))
}

async fn join_indices<H: Future<Output = Result<usize, String>>>(
    handles: Vec<H>,
) -> Result<u64, String> {
    let mut checksum = 0;
    for (i, handle) in handles.into_iter().enumerate() {
        let value = handle.await?;
        if value != i {
            return Err(format!("task {i} returned {value}"));
        }
        checksum += value as u64;
    }
    Ok(checksum)
}

fn batch<E: Engine>(rt: &mut E, tasks: usize, yields: usize) -> Result<u64, String> {
    let mut handles = Vec::with_capacity(tasks);
    for index in 0..tasks {
        handles.push(rt.spawn(YieldWork {
            remaining: yields,
            index,
        })?);
    }
    let checksum = rt.block_on(join_indices(handles))?;
    require_idle(rt)?;
    Ok(checksum)
}

fn require_idle(engine: &impl Engine) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(1);
    // A join can wake the caller before a background worker retires its
    // registry entry. Keep this drain boundary outside measured intervals.
    while engine.alive() != 0 {
        if Instant::now() >= deadline {
            return Err("tasks remain active after scenario".into());
        }
        std::thread::yield_now();
    }
    Ok(())
}

fn emit(value: &Value) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, value).map_err(|e| e.to_string())?;
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

fn soak<E: Engine>(c: &Config, mut rt: E) -> Result<Value, String> {
    let duration = Duration::from_secs(c.duration_secs.ok_or("missing soak duration")?);
    let started = Instant::now();
    let mut next_sample = Duration::ZERO;
    let mut cycles = 0_u64;
    let mut completed = 0_u64;
    let expected = c.tasks as u64 * (c.tasks as u64 - 1) / 2;
    let mut event = c.record("running");
    event["event"] = json!("start");
    event["rss"] = rss();
    emit(&event)?;
    while started.elapsed() < duration {
        if batch(&mut rt, c.tasks, c.yields)? != expected {
            return Err("soak checksum mismatch".into());
        }
        cycles += 1;
        completed += c.tasks as u64;
        let elapsed = started.elapsed();
        if elapsed >= next_sample {
            let mut sample = c.record("running");
            sample["event"] = json!("sample");
            sample["elapsed_secs"] = json!(elapsed.as_secs_f64());
            sample["cycles"] = json!(cycles);
            sample["completed_tasks"] = json!(completed);
            sample["active_tasks"] = json!(rt.alive());
            sample["rss"] = rss();
            emit(&sample)?;
            next_sample = elapsed + Duration::from_millis(c.sample_interval_ms);
        }
    }
    let mut result = c.record("ok");
    result["event"] = json!("finish");
    result["correctness"] = json!(true);
    result["cycles"] = json!(cycles);
    result["completed_tasks"] = json!(completed);
    result["active_tasks"] = json!(rt.alive());
    result["elapsed_secs"] = json!(started.elapsed().as_secs_f64());
    result["rss"] = rss();
    Ok(result)
}

#[cfg(target_os = "linux")]
fn process_cpu_time() -> Option<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is writable for a timespec. The process clock needs no
    // external descriptor and the call does not retain the pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    let seconds = u64::try_from(time.tv_sec).ok()?;
    let nanos = u32::try_from(time.tv_nsec).ok()?;
    (nanos < 1_000_000_000).then(|| Duration::new(seconds, nanos))
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_time() -> Option<Duration> {
    None
}

fn run<E: Engine>(c: &Config) -> Result<Value, String> {
    // Read the same RSS source before and after runtime construction.
    let process_rss = rss();
    let mut rt = E::new(c)?;
    rt.block_on(async {});
    let runtime_rss = rss();
    if c.scenario == "soak" {
        return soak(c, rt);
    }
    let mut result = c.record("ok");
    let metrics;
    let mut completed = c.tasks;
    let mut checksum = 0_u64;
    match c.scenario.as_str() {
        "spawn_join" => {
            let started = Instant::now();
            let mut handles = Vec::with_capacity(c.tasks);
            for index in 0..c.tasks {
                handles.push(rt.spawn(async move { index })?);
            }
            checksum = rt.block_on(join_indices(handles))?;
            let ns = started.elapsed().as_nanos().max(1) as u64;
            metrics = json!({"elapsed_ns": ns, "spawn_join_ops_per_sec": c.tasks as f64 * 1e9 / ns as f64});
        }
        "batch_join" => {
            let started = Instant::now();
            let handles = rt.spawn_batch((0..c.tasks).map(|index| YieldWork {
                remaining: c.yields,
                index,
            }))?;
            let submission_ns = started.elapsed().as_nanos().max(1) as u64;
            checksum = rt.block_on(join_indices(handles))?;
            let ns = started.elapsed().as_nanos().max(1) as u64;
            metrics = json!({"elapsed_ns": ns, "submission_ns": submission_ns,
                "batch_join_ops_per_sec": c.tasks as f64 * 1e9 / ns as f64});
        }
        "yield" => {
            let ready_start = Instant::now();
            let ready_cpu_start = (c.workers != 1).then(process_cpu_time).flatten();
            let mut handles = Vec::with_capacity(c.tasks);
            for index in 0..c.tasks {
                handles.push(rt.spawn(YieldWork {
                    remaining: c.yields,
                    index,
                })?);
            }
            let (started, cpu_started) = if c.workers == 1 {
                (Instant::now(), process_cpu_time())
            } else {
                (ready_start, ready_cpu_start)
            };
            checksum = rt.block_on(join_indices(handles))?;
            let ns = started.elapsed().as_nanos().max(1) as u64;
            let cpu_ns = cpu_started
                .and_then(|start| process_cpu_time()?.checked_sub(start))
                .and_then(|duration| u64::try_from(duration.as_nanos()).ok());
            let ops = c.tasks as u64 * c.yields as u64;
            metrics = json!({"elapsed_ns": ns, "wake_ops": ops,
                "wake_ops_per_sec": ops as f64 * 1e9 / ns as f64, "yield_ns_per_op": ns as f64 / ops as f64,
                "yield_process_cpu_ns": cpu_ns,
                "yield_cpu_ns_per_wake": cpu_ns.map(|time| time as f64 / ops as f64)});
        }
        "wake_latency" | "external_wake_latency" | "batch_external_wake_latency" => {
            let count = c.tasks.min(c.latency_samples);
            let (mut samples, prefix) = if c.scenario == "batch_external_wake_latency" {
                let (samples, overlapping) = batch_external_wake_samples(&mut rt, c.tasks, count)?;
                result["wakes_started_before_submission_returned"] = json!(overlapping);
                (samples, "batch_external_wake")
            } else if c.scenario == "external_wake_latency" {
                (
                    external_wake_samples(&mut rt, c.tasks, count)?,
                    "external_wake",
                )
            } else {
                let mut handles = Vec::with_capacity(c.tasks);
                for index in 0..c.tasks {
                    let sampled = index * count / c.tasks != (index + 1) * count / c.tasks;
                    handles.push(rt.spawn(WakeWork {
                        index,
                        sampled,
                        armed: false,
                        start: None,
                    })?);
                }
                (join_wake_samples(&mut rt, handles, count)?, "wake")
            };
            metrics = Value::Object(
                [("p50", 0.50), ("p95", 0.95), ("p99", 0.99)]
                    .into_iter()
                    .map(|(name, q)| {
                        (
                            format!("{prefix}_{name}_ns"),
                            json!(percentile(&mut samples, q)),
                        )
                    })
                    .collect(),
            );
            result["latency_samples_ns"] = json!(samples);
            checksum = c.tasks as u64 * (c.tasks as u64 - 1) / 2;
        }
        "pending_memory" => {
            let count = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::with_capacity(c.tasks);
            for _ in 0..c.tasks {
                handles.push(rt.spawn(PendingWork {
                    entered: false,
                    count: count.clone(),
                })?);
            }
            rt.block_on(poll_fn(|cx| {
                if count.load(Ordering::Relaxed) == c.tasks {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }));
            std::thread::sleep(Duration::from_millis(c.hold_ms));
            let live_rss = rss();
            let delta = rss_delta(&live_rss, &runtime_rss);
            metrics = json!({"pending_rss_delta_bytes": delta,
                "pending_bytes_per_task": delta.map(|v| v as f64 / c.tasks as f64)});
            result["pending_polled_tasks"] = json!(count.load(Ordering::Relaxed));
            result["join_handle_bytes"] = json!(std::mem::size_of::<E::Handle<()>>());
            result["live_rss"] = live_rss;
            for h in &handles {
                h.abort();
            }
            rt.block_on(async {
                for h in handles {
                    match h.await {
                        Err(error) if error == "cancelled" => {}
                        Err(error) => return Err(error),
                        Ok(()) => {
                            return Err("pending task completed without cancellation".to_string());
                        }
                    }
                }
                Ok(())
            })?;
            result["after_cleanup_rss"] = rss();
            result["cancelled_tasks"] = json!(c.tasks);
        }
        "idle" => {
            completed = 0;
            metrics = json!({"idle_rss_delta_bytes": rss_delta(&runtime_rss, &process_rss)});
        }
        _ => return Err("unhandled scenario".into()),
    }
    require_idle(&rt)?;
    result["metrics"] = metrics;
    result["correctness"] = json!(true);
    result["completed_tasks"] = json!(completed);
    result["checksum"] = json!(checksum);
    result["active_tasks"] = json!(rt.alive());
    result["process_rss"] = process_rss;
    result["runtime_rss"] = runtime_rss;
    Ok(result)
}

fn main() {
    let c = match Config::parse() {
        Ok(c) => c,
        Err(error) => {
            let _ = emit(&json!({"schema_version": 1, "status": "error", "error": error}));
            std::process::exit(2);
        }
    };
    let result = match (c.engine.as_str(), c.workers) {
        ("zrun", 1) if c.scenario != "batch_join" => run::<zrun::Runtime>(&c),
        ("zrun", _) => run::<zrun::Pool>(&c),
        _ => run::<tokio::runtime::Runtime>(&c),
    };
    match result {
        Ok(value) => {
            if let Err(e) = emit(&value) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            let mut r = c.record("error");
            r["error"] = json!(error);
            let _ = emit(&r);
            std::process::exit(1);
        }
    }
}
