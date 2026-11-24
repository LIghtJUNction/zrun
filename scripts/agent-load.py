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
LARGE_SCRIPT = """head -c 65536 /dev/zero | tr '\\000' '"'; head -c 65536 /dev/zero | tr '\\000' '"' >&2"""
LARGE_OUTPUT = {"encoding": "utf8", "data": '"' * 65536}


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


def command_post(index, arguments):
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": index,
            "method": "tools/call",
            "params": {"name": "shell_run", "arguments": arguments},
        },
        separators=(",", ":"),
    ).encode()
    return (
        f"POST /mcp HTTP/1.1\r\nHost: zrun\r\nAccept: application/json, text/event-stream\r\n"
        f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n".encode()
        + body
    )


async def response_length(reader):
    head = (await reader.readuntil(b"\r\n\r\n")).decode()
    assert head.split()[1] == "200", head
    headers = dict(line.split(":", 1) for line in head.splitlines()[1:] if line)
    return int(headers["Content-Length"])


async def request(path, index, mode):
    arguments = {"script": SCRIPT}
    if mode == "reuse":
        arguments["reuse"] = REUSE
    reader, writer = await asyncio.open_unix_connection(path)
    try:
        writer.write(command_post(index, arguments))
        await writer.drain()
        result = json.loads(await reader.readexactly(await response_length(reader)))
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



async def slow_output(path, pid, agents):
    arguments = {"script": LARGE_SCRIPT, "reuse": REUSE}

    async def open_request(index):
        reader, writer = await asyncio.open_unix_connection(path, limit=1024)
        try:
            writer.write(command_post(index, arguments))
            await writer.drain()
            return reader, writer, await response_length(reader)
        except BaseException:
            writer.close()
            await writer.wait_closed()
            raise

    async def verify(index, connection, disposition):
        reader, _, length = connection
        rpc = json.loads(await reader.readexactly(length))
        assert rpc["id"] == index and not rpc["result"]["isError"]
        delivery = rpc["result"]["structuredContent"]
        assert json.loads(rpc["result"]["content"][0]["text"]) == delivery
        assert delivery["disposition"] == disposition
        result = delivery["result"]
        assert result["exit_code"] == 0 and not result["truncated"]
        assert result["stdout"] == result["stderr"] == LARGE_OUTPUT
        return length

    warm = await open_request(0)
    try:
        await verify(0, warm, "executed")
    finally:
        warm[1].close()
        await warm[1].wait_closed()
    idle_rss = process_sample(pid)[0]
    connections = []
    ready = asyncio.Event()
    arrived = asyncio.Queue()

    async def slow_client(index):
        connection = await open_request(index)
        connections.append(connection)
        arrived.put_nowait(None)
        await ready.wait()
        return await verify(index, connection, "cached")

    start = time.monotonic()
    try:
        async with asyncio.TaskGroup() as group:
            calls = [group.create_task(slow_client(index)) for index in range(1, agents + 1)]
            for _ in calls:
                await arrived.get()
            headers_s = time.monotonic() - start
            # The transport pauses once its buffer exceeds 2 KiB. Allow one
            # read to overshoot, then wait for the server to block on writing.
            await asyncio.sleep(0.10)
            held_rss, threads, children = process_sample(pid)
            ready.set()
        lengths = [call.result() for call in calls]
    finally:
        for _, writer, _ in connections:
            writer.close()
        await asyncio.gather(*(writer.wait_closed() for _, writer, _ in connections))
    assert children == 0
    return {
        "mode": "slow-output",
        "agents": agents,
        "headers_s": headers_s,
        "elapsed_s": time.monotonic() - start,
        "warm_rss_kib": idle_rss,
        "held_rss_kib": held_rss,
        "held_threads": threads,
        "held_children": children,
        "verified_response_bytes": sum(lengths),
    }


async def slow_input(path, pid, agents):
    directory = Path(f"/proc/{pid}/fd")
    idle_fds = len(list(directory.iterdir()))
    idle_rss = process_sample(pid)[0]
    body_size = 256 * 1024
    headers = (
        "POST /mcp HTTP/1.1\r\nHost: zrun\r\nContent-Type: application/json\r\n"
        f"Content-Length: {body_size}\r\n\r\n"
    ).encode()
    clients = []

    async def open_client():
        reader, writer = await asyncio.open_unix_connection(path)
        clients.append((reader, writer))
        writer.write(headers)
        await writer.drain()

    try:
        await asyncio.gather(*(open_client() for _ in range(agents)))
        while (held_fds := len(list(directory.iterdir()))) < idle_fds + agents:
            await asyncio.sleep(0.01)
        await asyncio.sleep(0.1)
        held_rss, threads, children = process_sample(pid)
        # Confirm the stalled connections can finish with exactly the advertised body.
        verified = min(8, agents)
        for index, (reader, writer) in enumerate(clients[:verified]):
            body = json.dumps({"jsonrpc": "2.0", "id": index, "method": "ping"}).encode()
            writer.write(body.ljust(body_size, b" "))
            await writer.drain()
            reply = json.loads(await reader.readexactly(await response_length(reader)))
            assert reply == {"jsonrpc": "2.0", "id": index, "result": {}}, reply
    finally:
        for _, writer in clients:
            writer.close()
        await asyncio.gather(*(writer.wait_closed() for _, writer in clients))
    assert children == 0
    return {
        "mode": "slow-input",
        "agents": agents,
        "advertised_body_bytes": body_size,
        "verified_responses": verified,
        "idle_rss_kib": idle_rss,
        "held_rss_kib": held_rss,
        "held_fds": held_fds,
        "held_threads": threads,
        "held_children": children,
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
            if mode in ("slow-input", "slow-output"):
                stalled = slow_input if mode == "slow-input" else slow_output
                return asyncio.run(asyncio.wait_for(stalled(path, process.pid, agents), timeout=60))
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
    comparisons = parser.add_mutually_exclusive_group()
    comparisons.add_argument("--slow-input-baseline", type=Path, help="compare partial request memory with this baseline daemon")
    comparisons.add_argument("--slow-output-baseline", type=Path, help="compare cached large output with this baseline daemon")
    parser.add_argument("--workers", type=int, choices=range(1,17), default=1)
    args = parser.parse_args()
    if not 1 <= args.agents <= 2_000 or args.rounds < 1:
        parser.error("agents must be 1..2000 and rounds must be positive")
    baseline = args.slow_input_baseline or args.slow_output_baseline
    if baseline:
        mode = "slow-input" if args.slow_input_baseline else "slow-output"
        order = [("candidate", args.binary), ("baseline", baseline)]
        for round_number in range(args.rounds):
            binaries = order if round_number % 2 == 0 else reversed(order)
            for version, binary in binaries:
                sample = run(binary.resolve(), args.agents, mode, args.workers)
                print(json.dumps({"round": round_number + 1, "version": version,
                                  "workers": args.workers, **sample}), flush=True)
        return
    for round_number in range(args.rounds):
        modes = ("live", "reuse") if round_number % 2 == 0 else ("reuse", "live")
        for mode in modes:
            sample = run(args.binary.resolve(), args.agents, mode, args.workers)
            print(json.dumps({"round": round_number + 1, "workers": args.workers, **sample}), flush=True)


if __name__ == "__main__":
    main()
