//! An echo server and a common load generator, each run in a fresh process.

use std::future::Future;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Barrier;
use zrun::io::{Poller, Stream};
use zrun::{Pool, Runtime};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;
const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const MAX_LATENCY_SAMPLES: usize = 16_384;
const MAX_SAMPLES_PER_CONNECTION: usize = 2_048;

#[derive(Clone, Copy)]
struct Workload {
    connections: usize,
    payload: usize,
    rounds: usize,
    warmup: usize,
}

impl Workload {
    fn parse(values: &[String]) -> Result<Self> {
        let [connections, payload, rounds, warmup] = values else {
            return Err("expected CONNECTIONS PAYLOAD ROUNDS WARMUP".into());
        };
        let work = Self {
            connections: connections.parse()?,
            payload: payload.parse()?,
            rounds: rounds.parse()?,
            warmup: warmup.parse()?,
        };
        if !(1..=4_096).contains(&work.connections)
            || !(16..=65_536).contains(&work.payload)
            || !(1..=1_048_576).contains(&work.rounds)
            || work.warmup > 256
            || work.messages() > 1_048_576
            || work.messages() as u64 * work.payload as u64 > 4 * 1024 * 1024 * 1024
        {
            return Err("TCP workload outside supported bounds".into());
        }
        Ok(work)
    }

    fn messages(self) -> usize {
        self.connections.saturating_mul(self.rounds)
    }

    fn all_messages(self) -> usize {
        self.connections * (self.rounds + self.warmup)
    }

    fn sample_stride(self) -> usize {
        let per_connection =
            (MAX_LATENCY_SAMPLES / self.connections).min(MAX_SAMPLES_PER_CONNECTION);
        self.rounds.div_ceil(per_connection)
    }

    fn record(self) -> Value {
        json!({
            "connections": self.connections,
            "payload": self.payload,
            "rounds": self.rounds,
            "warmup": self.warmup,
            "nodelay": true
        })
    }
}

fn emit(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn build() -> Value {
    json!({
        "rustc": env!("ZRUN_BENCH_RUSTC"),
        "profile": env!("ZRUN_BENCH_PROFILE"),
        "target": env!("ZRUN_BENCH_TARGET"),
        "opt_level": env!("ZRUN_BENCH_OPT_LEVEL"),
        "encoded_rustflags": env!("ZRUN_BENCH_CARGO_ENCODED_RUSTFLAGS"),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn cpu_ticks() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let mut fields = stat.rsplit_once(") ")?.1.split_ascii_whitespace().skip(11);
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

fn elapsed_ticks(before: Option<(u64, u64)>) -> Option<(u64, u64)> {
    let (user, system) = before?;
    let after = cpu_ticks()?;
    Some((after.0 - user, after.1 - system))
}

trait EchoStream {
    async fn receive(&mut self, bytes: &mut [u8]) -> io::Result<()>;
    async fn send(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl EchoStream for Stream {
    async fn receive(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.read_exact(bytes).await
    }

    async fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes).await
    }
}

impl EchoStream for TcpStream {
    async fn receive(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.read_exact(bytes).await.map(|_| ())
    }

    async fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes).await
    }
}

async fn echo(mut stream: impl EchoStream, work: Workload) -> io::Result<()> {
    let mut bytes = vec![0; work.payload];
    for _ in 0..work.rounds + work.warmup {
        stream.receive(&mut bytes).await?;
        stream.send(&bytes).await?;
    }
    Ok(())
}

fn ready(addr: SocketAddr) -> Result<()> {
    emit(&json!({"phase":"ready", "address":addr.to_string()}))
}

async fn serve_native(
    mut listener: zrun::io::Listener,
    work: Workload,
    mut spawn: impl FnMut(
        zrun::io::Stream,
    )
        -> std::result::Result<zrun::JoinHandle<io::Result<()>>, zrun::SpawnError>,
) -> Result<()> {
    let mut accepted = Vec::with_capacity(work.connections);
    for _ in 0..work.connections {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        accepted.push(spawn(stream)?);
    }
    wait_for_echoes(accepted).await
}

async fn wait_for_echoes<H, E>(accepted: Vec<H>) -> Result<()>
where
    H: Future<Output = std::result::Result<io::Result<()>, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    for echo in accepted {
        echo.await??;
    }
    Ok(())
}

fn serve_zrun(workers: usize, work: Workload) -> Result<()> {
    if workers == 1 {
        let mut poller = Poller::new()?;
        let listener = poller.handle().bind(LOOPBACK)?;
        let mut runtime = Runtime::with_max_tasks(work.connections)?;
        let spawn = runtime.spawner();
        ready(listener.local_addr()?)?;
        runtime.block_on_with_io(
            &mut poller,
            serve_native(listener, work, |stream| spawn.spawn(echo(stream, work))),
        )?
    } else {
        let (mut runtime, io) = Pool::with_max_tasks_and_io(workers, work.connections)?;
        let listener = io.bind(LOOPBACK)?;
        let spawn = runtime.spawner();
        ready(listener.local_addr()?)?;
        runtime.block_on(serve_native(listener, work, |stream| {
            spawn.spawn(echo(stream, work))
        }))
    }
}

fn tokio_runtime(workers: usize) -> io::Result<tokio::runtime::Runtime> {
    let mut runtime = if workers == 1 {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        runtime.worker_threads(workers);
        runtime
    };
    runtime.enable_io().build()
}

fn serve_tokio(workers: usize, work: Workload) -> Result<()> {
    tokio_runtime(workers)?.block_on(async {
        let listener = TcpListener::bind(LOOPBACK).await?;
        ready(listener.local_addr()?)?;
        let mut accepted = Vec::with_capacity(work.connections);
        for _ in 0..work.connections {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            accepted.push(tokio::spawn(echo(stream, work)));
        }
        wait_for_echoes(accepted).await
    })
}

struct Client {
    stream: TcpStream,
    sent: Vec<u8>,
    received: Vec<u8>,
    sequence: u64,
}

impl Client {
    async fn connect(addr: SocketAddr, id: usize, work: Workload) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let mut sent = vec![0xa5; work.payload];
        sent[..8].copy_from_slice(&(id as u64).to_le_bytes());
        let mut client = Self {
            stream,
            sent,
            received: vec![0; work.payload],
            sequence: 0,
        };
        for _ in 0..work.warmup {
            client.roundtrip().await?;
        }
        Ok(client)
    }

    async fn roundtrip(&mut self) -> Result<()> {
        self.sent[8..16].copy_from_slice(&self.sequence.to_le_bytes());
        self.sequence += 1;
        self.stream.send(&self.sent).await?;
        self.stream.receive(&mut self.received).await?;
        if self.sent != self.received {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "TCP echo payload mismatch").into(),
            );
        }
        Ok(())
    }
}

async fn drive(addr: SocketAddr, workers: usize, work: Workload) -> Result<Value> {
    // Establish and warm every socket before releasing the measured traffic.
    let mut clients = Vec::with_capacity(work.connections);
    for id in 0..work.connections {
        clients.push(Client::connect(addr, id, work).await?);
    }
    let stride = work.sample_stride();
    let start = Arc::new(Barrier::new(work.connections + 1));
    let mut tasks = Vec::with_capacity(work.connections);
    for mut client in clients {
        let start = start.clone();
        tasks.push(tokio::spawn(async move {
            let mut samples = Vec::with_capacity(work.rounds.div_ceil(stride));
            start.wait().await;
            for round in 0..work.rounds {
                let timed = (round % stride == 0).then(Instant::now);
                client.roundtrip().await?;
                if let Some(timed) = timed {
                    samples.push(u64::try_from(timed.elapsed().as_nanos())?);
                }
            }
            Ok::<_, Error>((samples, Instant::now()))
        }));
    }
    let before = cpu_ticks();
    let begun = Instant::now();
    start.wait().await;
    let mut samples = Vec::with_capacity(work.connections * work.rounds.div_ceil(stride));
    let mut finished = begun;
    for task in tasks {
        let (mut local, completion) = task.await??;
        samples.append(&mut local);
        finished = finished.max(completion);
    }
    Ok(json!({
        "phase":"finished", "workers":workers, "workload":work.record(), "build":build(),
        "messages":work.messages(), "elapsed_ns":u64::try_from(finished.duration_since(begun).as_nanos())?,
        "latency_samples_ns":samples, "sample_stride":stride, "cpu_ticks":elapsed_ticks(before),
    }))
}

fn run(args: &[String]) -> Result<()> {
    let [mode, endpoint, workers, workload @ ..] = args else {
        return Err("usage: zrun-tcp-bench server ENGINE WORKERS CONNECTIONS PAYLOAD ROUNDS WARMUP | client ADDRESS WORKERS CONNECTIONS PAYLOAD ROUNDS WARMUP".into());
    };
    let work = Workload::parse(workload)?;
    let workers = workers.parse::<usize>()?;
    if !(1..=16).contains(&workers) {
        return Err("worker count must be 1..16".into());
    }
    match (mode.as_str(), endpoint.as_str()) {
        ("server", "zrun" | "tokio") => {
            let started = cpu_ticks();
            match endpoint.as_str() {
                "zrun" => serve_zrun(workers, work)?,
                _ => serve_tokio(workers, work)?,
            }
            emit(&json!({
                "phase":"finished", "engine":endpoint, "workers":workers,
                "workload":work.record(), "build":build(), "messages":work.all_messages(),
                "cpu_ticks":elapsed_ticks(started),
            }))
        }
        ("client", addr) => {
            let address = addr.parse::<SocketAddr>()?;
            if !address.ip().is_loopback() {
                return Err("TCP benchmark client requires a loopback address".into());
            }
            let result = tokio_runtime(workers)?.block_on(drive(address, workers, work))?;
            emit(&result)
        }
        _ => Err("invalid TCP benchmark mode or engine".into()),
    }
}

fn main() -> std::process::ExitCode {
    match run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zrun-tcp-bench: {}", format_args!("{error}"));
            std::process::ExitCode::FAILURE
        }
    }
}
