#!/usr/bin/env python3
"""Pair independent loopback echo servers with an identical TCP load generator."""

import argparse
import ipaddress
import json
import math
import os
from pathlib import Path
import subprocess
import time

import benchmark as bench

DEFAULT_CASES = ((1, 64, 12_000), (64, 64, 800), (128, 4096, 400), (32, 65_536, 150))
MAX_RAW_BYTES = 512 * 1024


def parse_case(text):
    try:
        connections, payload, rounds = map(int, text.split(":"))
        if (1 <= connections <= 4096 and 16 <= payload <= 65_536
                and 1 <= connections * rounds <= 1_048_576
                and connections * rounds * payload <= 4 * 1024 ** 3):
            return connections, payload, rounds
    except ValueError:
        pass
    raise argparse.ArgumentTypeError("expected bounded CONNECTIONS:PAYLOAD:ROUNDS")


def validate_affinity(text):
    if not text:
        return None
    if not hasattr(os, "sched_setaffinity"):
        raise ValueError("CPU affinity is not available on this OS")
    cpus = bench.parse_affinity(text)
    if not set(cpus).issubset(bench.affinity()):
        raise ValueError(f"CPUs unavailable in {text}")
    return cpus


def start(command, stdout, stderr, cpus):
    if cpus:
        command = ["taskset", "-c", ",".join(map(str, cpus)), *command]
    with stdout.open("wb") as out, stderr.open("wb") as err:
        return subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=out,
                                stderr=err, start_new_session=os.name == "posix")


def bounded_read(path):
    if path.stat().st_size > MAX_RAW_BYTES:
        raise ValueError(f"{path.name} exceeds the raw sample size bound")
    return path.read_text(encoding="utf-8")


def check_deadline(deadline):
    if bench.STOP_REQUESTED:
        raise InterruptedError("TCP benchmark cancelled")
    if time.monotonic() >= deadline:
        raise TimeoutError("TCP benchmark sample timed out")


def wait_ready(process, path, deadline):
    while True:
        data = bounded_read(path)
        if "\n" in data:
            ready = json.loads(data.split("\n", 1)[0])
            addr, port = ready["address"].rsplit(":", 1)
            if (ready.get("phase") != "ready" or not ipaddress.IPv4Address(addr).is_loopback
                    or not 0 < int(port) <= 65535):
                raise ValueError("invalid ready record")
            return ready["address"]
        if process.poll() is not None:
            raise ValueError(f"server exited before listening, status {process.returncode}")
        check_deadline(deadline)
        time.sleep(0.005)


def validate_result(args, case, workers, engine, client, server):
    connections, payload, rounds = case
    expected = {"connections": connections, "payload": payload, "rounds": rounds,
                "warmup": args.warmup, "nodelay": True}
    if (server.get("phase") != "finished" or client.get("phase") != "finished"
            or server.get("workload") != expected or client.get("workload") != expected
            or server.get("engine") != engine or server.get("workers") != workers
            or client.get("workers") != args.client_workers or client["build"] != server["build"]
            or server.get("messages") != connections * (rounds + args.warmup)
            or client.get("messages") != connections * rounds):
        raise ValueError("TCP workload or completion accounting mismatch")
    stride = client["sample_stride"]
    elapsed = client["elapsed_ns"]
    latency = client["latency_samples_ns"]
    expected_stride = math.ceil(rounds / min(2048, 16_384 // connections))
    if (stride != expected_stride or not isinstance(elapsed, int) or elapsed <= 0
            or len(latency) != connections * math.ceil(rounds / stride)
            or not all(isinstance(x, int) and 0 < x <= elapsed for x in latency)):
        raise ValueError("TCP latency sampling mismatch")
    return {
        "status": "ok", "profile": client["build"]["profile"],
        "messages": client["messages"], "elapsed_ns": elapsed, "latency_samples": len(latency),
        "server_cpu_ticks": server["cpu_ticks"], "client_cpu_ticks": client["cpu_ticks"],
        "metrics": {
            "tcp_bytes_per_second": connections * rounds * payload * 1e9 / elapsed,
            "tcp_rtt_p50_ns": bench.quantile(latency, 0.5),
            "tcp_rtt_p95_ns": bench.quantile(latency, 0.95),
            "tcp_rtt_p99_ns": bench.quantile(latency, 0.99),
        },
    }


def sample(args, out, pair, workers, case, engine):
    name = f"r{pair:02d}-w{workers}-c{case[0]}-b{case[1]}-n{case[2]}-{engine}"
    server_out, server_err, client_out, client_err = (
        out / f"{name}-{suffix}" for suffix in
        ("server.jsonl", "server.stderr", "client.json", "client.stderr")
    )
    server = client = None
    record = {"pair": pair, "workers": workers, "case": case, "engine": engine,
              "raw": [p.name for p in (server_out, server_err, client_out, client_err)],
              "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None}
    try:
        deadline = time.monotonic() + args.timeout
        workload = list(map(str, (*case, args.warmup)))
        server = start([args.binary, "server", engine, str(workers), *workload],
                       server_out, server_err, args.server_cpus)
        address = wait_ready(server, server_out, deadline)
        client = start([args.binary, "client", address, str(args.client_workers), *workload],
                       client_out, client_err, args.client_cpus)
        while client.poll() is None or server.poll() is None:
            if any(p.returncode not in (None, 0) for p in (client, server)):
                break
            check_deadline(deadline)
            time.sleep(0.005)
        if client.returncode != 0 or server.returncode != 0:
            raise ValueError(f"server status {server.returncode}, client status {client.returncode}")
        _, complete = bounded_read(server_out).splitlines()
        record.update(validate_result(args, case, workers, engine,
                                      json.loads(bounded_read(client_out)), json.loads(complete)))
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        record.update(status="failed", error=str(error))
    finally:
        for p in (client, server):
            if p is not None:
                bench.stop_process(p)
    return record


def summarize(args, samples):
    rows = []
    config = (
        ("tcp_bytes_per_second", "higher", 0.95, 1.20),
        ("tcp_rtt_p99_ns", "lower", 1.10, 0.80),
    )
    for workers in args.workers:
        for case in args.case:
            found = {(s["pair"], s["engine"]): s for s in samples if s["workers"] == workers
                     and s["case"] == case and s["status"] == "ok"}
            for metric, direction, normal, challenge in config:
                values = {engine: [found.get((i, engine), {}).get("metrics", {}).get(metric)
                                   for i in range(args.rounds)] for engine in ("zrun", "tokio")}
                seed = bench.stable_seed(args.seed, workers, case, metric)
                rows.append({
                    "workers": workers, "case": case, "metric": metric,
                    "engines": {engine: bench.describe(seq, args.bootstrap, seed)
                                for engine, seq in values.items()},
                    **bench.compare_pairs(list(zip(values["zrun"], values["tokio"])),
                                          direction=direction, normal=normal, challenge=challenge,
                                          expected=args.rounds, iterations=args.bootstrap, seed=seed,
                                          release=all(s["profile"] == "release" for s in found.values())),
                })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, nargs="+", choices=bench.WORKERS, default=[1, 4])
    parser.add_argument("--client-workers", type=int, choices=bench.WORKERS, default=4)
    parser.add_argument("--case", type=parse_case, action="append", metavar="CONNECTIONS:PAYLOAD:ROUNDS")
    parser.add_argument("--warmup", type=int, default=8)
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--bootstrap", type=int, default=5000)
    parser.add_argument("--seed", type=int, default=20261124)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--server-affinity")
    parser.add_argument("--client-affinity")
    args = parser.parse_args()
    if (not 0 <= args.warmup <= 256 or not 1 <= args.rounds <= 100
            or not 100 <= args.bootstrap <= 100_000 or not 0 < args.timeout <= 3600):
        parser.error("warmup, rounds, bootstrap or timeout outside supported bounds")
    args.binary = args.binary.resolve()
    args.case = list(dict.fromkeys(args.case or DEFAULT_CASES))
    args.workers = list(dict.fromkeys(args.workers))
    try:
        args.server_cpus = validate_affinity(args.server_affinity)
        args.client_cpus = validate_affinity(args.client_affinity)
        if args.server_cpus and args.client_cpus and set(args.server_cpus) & set(args.client_cpus):
            raise ValueError("server and client affinities overlap")
        if not args.binary.is_file():
            raise ValueError(f"TCP comparator binary missing at {args.binary}")
        args.output.mkdir(parents=True, exist_ok=False)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    bench.install_signals()
    report = {"config": {key: str(value) if isinstance(value, Path) else value
                         for key, value in vars(args).items()},
              "environment": bench.environment(args.binary), "samples": [], "status":"running",
              "cpu_ticks_per_second": os.sysconf("SC_CLK_TCK") if os.name == "posix" else None}
    path = args.output / "report.json"
    bench.atomic_json(path, report)
    for pair in range(args.rounds):
        for workers in args.workers:
            for case in args.case:
                for engine in ("zrun", "tokio") if pair % 2 == 0 else ("tokio", "zrun"):
                    if bench.STOP_REQUESTED:
                        break
                    record = sample(args, args.output, pair, workers, case, engine)
                    report["samples"].append(record)
                    print(f"round={pair + 1}/{args.rounds} workers={workers} case={case} "
                          f"engine={engine} status={record['status']}", flush=True)
        bench.atomic_json(path, report)
        if bench.STOP_REQUESTED:
            break
    failed = any(s["status"] != "ok" for s in report["samples"])
    report["status"] = "cancelled" if bench.STOP_REQUESTED else "failed" if failed else "completed"
    report["summary"] = summarize(args, report["samples"])
    bench.atomic_json(path, report)
    print(path)
    return 130 if bench.STOP_REQUESTED else int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
