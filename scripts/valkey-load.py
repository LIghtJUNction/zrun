#!/usr/bin/env python3
"""Compare disabled, cold and cross-broker warm Valkey result caching on Linux."""

import argparse
import asyncio
from collections import Counter
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time

spec = importlib.util.spec_from_file_location("agent_load", Path(__file__).with_name("agent-load.py"))
agent_load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent_load)


@contextmanager
def broker(binary, root, env):
    path = root / "broker.sock"
    server = subprocess.Popen([binary, "mcp", "--unix", path], cwd=root, env=env,
                              stderr=subprocess.PIPE)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(server.stderr, selectors.EVENT_READ)
            if not selector.select(timeout=5):
                raise TimeoutError("broker did not announce readiness")
        assert server.stderr.readline().strip() == f"zrun: unix:{path}".encode()
        yield path, server.pid
    finally:
        server.terminate()
        server.wait(timeout=5)
        assert server.returncode == 0 and not path.exists()


async def workload(path, pid, agents, revision):
    async def call(index):
        reader, writer = await asyncio.open_unix_connection(path)
        try:
            arguments = {
                "script": "sleep .015; printf x >> count; printf zrun",
                "reuse": {"scope": "valkey-load", "input_revision": f"{revision}-{index}",
                          "ttl_ms": 60000},
            }
            writer.write(agent_load.command_post(index, arguments))
            await writer.drain()
            body = json.loads(await reader.readexactly(await agent_load.response_length(reader)))
            assert body["id"] == index and not body["result"]["isError"]
            delivery = body["result"]["structuredContent"]
            assert delivery["result"]["stdout"] == {"encoding": "utf8", "data": "zrun"}
            assert delivery["result"]["exit_code"] == 0
            return delivery["disposition"]
        finally:
            writer.close()
            await writer.wait_closed()

    samples = [agent_load.process_sample(pid)]

    async def sample():
        while True:
            await asyncio.sleep(.01)
            samples.append(agent_load.process_sample(pid))

    sampler = asyncio.create_task(sample())
    started = time.monotonic()
    try:
        dispositions = Counter(await asyncio.gather(*(call(index) for index in range(agents))))
    finally:
        sampler.cancel()
        await asyncio.gather(sampler, return_exceptions=True)
    return {
        "elapsed_s": time.monotonic() - started,
        "dispositions": dispositions,
        "peak_broker_rss_kib": max(row[0] for row in samples),
        "peak_direct_children": max(row[2] for row in samples),
    }


def run_case(binary, root, env, agents, revision, expected):
    counter = root / "count"
    counter.write_bytes(b"")
    with broker(binary, root, env) as (path, pid):
        result = asyncio.run(asyncio.wait_for(workload(path, pid, agents, revision), timeout=60))
    assert counter.read_bytes() == b"x" * (agents if expected == "executed" else 0)
    assert result["dispositions"] == {expected: agents}, result
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--agents", type=int, default=500, choices=range(1, 1001), metavar="1..1000")
    parser.add_argument("--rounds", type=int, default=10, choices=range(1, 101), metavar="1..100")
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="zrun-valkey-load.") as directory:
        root = Path(directory)
        socket = root / "valkey.sock"
        valkey = subprocess.Popen(
            ["valkey-server", "--port", "0", "--unixsocket", socket, "--unixsocketperm", "700",
             "--save", "", "--appendonly", "no", "--dir", root],
            stdout=subprocess.DEVNULL,
        )
        try:
            deadline = time.monotonic() + 5
            while not socket.is_socket():
                if valkey.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Valkey did not start")
                time.sleep(.02)
            base = {key: value for key, value in os.environ.items() if not key.startswith("ZRUN_")}
            base.update(ZRUN_SHELL="/bin/sh", ZRUN_WORKERS="4")
            enabled = dict(base, ZRUN_VALKEY_SOCKET=str(socket),
                           ZRUN_VALKEY_DOMAIN="load", ZRUN_VALKEY_TOOL_REVISION="fixture-v1")
            for pair in range(args.rounds):
                revision = f"round-{pair}"
                run_case(binary, root, enabled, args.agents, revision + "-warm", "executed")
                modes = ("disabled", "cold", "warm")
                for mode in modes[pair % 3:] + modes[:pair % 3]:
                    result = run_case(binary, root, base if mode == "disabled" else enabled,
                                      args.agents, revision + "-" + mode,
                                      "cached" if mode == "warm" else "executed")
                    print(json.dumps(dict(pair=pair, mode=mode, agents=args.agents, **result)),
                          flush=True)
        finally:
            valkey.terminate()
            valkey.wait(timeout=5)


if __name__ == "__main__":
    main()
