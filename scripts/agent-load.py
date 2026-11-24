#!/usr/bin/env python3
"""Compare the same Linux shell workload with and without shared reuse."""

import argparse
import asyncio
from collections import Counter
import json
import os
from pathlib import Path
import selectors
import statistics
import subprocess
import tempfile
import time

SCRIPT = "sleep 0.05; printf zrun"
REUSE = {"scope": "agent-load", "input_revision": "fixed-v1", "ttl_ms": 30_000}


def process_sample(pid):
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        fields = dict(line.split(":", 1) for line in status.splitlines())
        children = set()
        for worker in Path(f"/proc/{pid}/task").iterdir():
            try:
                children.update((worker / "children").read_text().split())
            except FileNotFoundError:
                continue
        return (
            int(fields["VmRSS"].split()[0]),
            int(fields["Threads"]),
            len(children),
        )
    except (FileNotFoundError, ProcessLookupError):
        return (0, 0, 0)


async def request(path, index, mode):
    arguments = {"script": SCRIPT}
    if mode == "reuse":
        arguments["reuse"] = REUSE
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": index,
            "method": "tools/call",
            "params": {"name": "shell_run", "arguments": arguments},
        },
        separators=(",", ":"),
    ).encode()
    reader, writer = await asyncio.open_unix_connection(path)
    try:
        writer.write(
            f"POST /mcp HTTP/1.1\r\nHost: zrun\r\nAccept: application/json, text/event-stream\r\n"
            f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n".encode()
            + body
        )
        await writer.drain()
        head = (await reader.readuntil(b"\r\n\r\n")).decode()
        assert head.split()[1] == "200", head
        headers = dict(line.split(":", 1) for line in head.splitlines()[1:] if line)
        result = json.loads(await reader.readexactly(int(headers["Content-Length"])))
        assert result["id"] == index and not result["result"]["isError"], result
        delivery = result["result"]["structuredContent"]
        assert delivery["result"]["stdout"] == {"encoding": "utf8", "data": "zrun"}, delivery
        return delivery["disposition"]
    finally:
        writer.close()
        await writer.wait_closed()


async def load(path, pid, agents, mode):
    idle_rss, _, _ = process_sample(pid)
    release = asyncio.Event()
    start = None
    samples = [process_sample(pid)]

    async def sample():
        while True:
            samples.append(process_sample(pid))
            await asyncio.sleep(0.005)

    async def one(index):
        await release.wait()
        disposition = await request(path, index, mode)
        return disposition, (time.monotonic() - start) * 1000

    monitor = asyncio.create_task(sample())
    tasks = [asyncio.create_task(one(index)) for index in range(agents)]
    await asyncio.sleep(0)
    start = time.monotonic()
    release.set()
    try:
        results = await asyncio.wait_for(asyncio.gather(*tasks), timeout=60)
    finally:
        monitor.cancel()
        await asyncio.gather(monitor, return_exceptions=True)
    elapsed = time.monotonic() - start
    counts = Counter(kind for kind, _ in results)
    latencies = sorted(ms for _, ms in results)
    assert counts["executed"] == (agents if mode == "live" else 1), counts
    if mode == "reuse":
        assert counts["coalesced"] + counts["cached"] == agents - 1, counts
    return {
        "mode": mode,
        "agents": agents,
        "elapsed_s": elapsed,
        "requests_s": agents / elapsed,
        "p50_ms": statistics.median(latencies),
        "p99_ms": latencies[(99 * (agents - 1)) // 100],
        "idle_rss_kib": idle_rss,
        "sampled_peak_rss_kib": max(x[0] for x in samples),
        "sampled_peak_threads": max(x[1] for x in samples),
        "sampled_peak_direct_children": max(x[2] for x in samples),
        "dispositions": counts,
    }


def run(binary, agents, mode, workers):
    with tempfile.TemporaryDirectory(prefix="zrun-load-") as directory:
        path = Path(directory) / "run.sock"
        env = {key: value for key, value in os.environ.items() if not key.startswith("ZRUN_")}
        env["ZRUN_SHELL"] = "/bin/sh"
        env["ZRUN_WORKERS"] = str(workers)
        process = subprocess.Popen(
            [binary, "mcp", "--unix", path],
            stderr=subprocess.PIPE,
            env=env,
        )
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stderr, selectors.EVENT_READ)
                if not selector.select(timeout=5):
                    raise TimeoutError("zrun daemon did not announce readiness")
                announced = process.stderr.readline()
            assert announced.strip() == f"zrun: unix:{path}".encode(), announced
            return asyncio.run(load(path, process.pid, agents, mode))
        finally:
            process.terminate()
            process.wait(timeout=5)
            assert process.returncode == 0 and not path.exists(), process.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--agents", type=int, default=1_000)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--workers", type=int, choices=range(1,17), default=1)
    args = parser.parse_args()
    if not 1 <= args.agents <= 2_000 or args.rounds < 1:
        parser.error("agents must be 1..2000 and rounds must be positive")
    for round_number in range(args.rounds):
        modes = ("live", "reuse") if round_number % 2 == 0 else ("reuse", "live")
        for mode in modes:
            sample = run(args.binary.resolve(), args.agents, mode, args.workers)
            print(json.dumps({"round": round_number + 1, "workers": args.workers, **sample}), flush=True)


if __name__ == "__main__":
    main()
