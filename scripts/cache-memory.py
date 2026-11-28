#!/usr/bin/env python3
"""Bounded Linux daemon cache probe; synthetic data only, one request at a time."""
import argparse
import json
import os
from pathlib import Path
import resource
import signal
import socket
import subprocess
import tempfile
import threading
import time


def memory(pid):
    try:
        fields = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
        return int(fields['VmRSS'].split()[0]), int(fields['VmHWM'].split()[0])
    except (FileNotFoundError, ProcessLookupError, KeyError):
        return 0, 0


def descendants(pid):
    # Some cloud kernels omit task/*/children; use process PPid instead.
    parents = {}
    for status in Path('/proc').glob('[0-9]*/status'):
        try:
            fields = dict(line.split(':', 1) for line in status.read_text().splitlines() if ':' in line)
            parents[int(status.parent.name)] = int(fields['PPid'])
        except (FileNotFoundError, ProcessLookupError):
            pass
    found = set()
    pending = [pid]
    while pending:
        current = pending.pop()
        children = {child for child, parent in parents.items() if parent == current} - found
        found.update(children)
        pending.extend(children)
    return found


def limits():
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2,) * 2)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def probe(binary):
    with tempfile.TemporaryDirectory(prefix='zrun-cache-') as directory:
        endpoint = str(Path(directory) / 'daemon.sock')
        env = {k: v for k, v in os.environ.items() if not k.startswith('ZRUN_')}
        env.update(ZRUN_SHELL='/bin/sh', ZRUN_WORKERS='1')
        server = subprocess.Popen([binary, 'mcp', '--unix', endpoint], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  preexec_fn=limits, start_new_session=True)
        stop = threading.Event()
        samples = []
        aborted = []
        started = time.monotonic()

        def monitor():
            while not stop.is_set():
                own, high = memory(server.pid)
                children = descendants(server.pid)
                child_rss = sum(memory(pid)[0] for pid in children)
                samples.append((time.monotonic() - started, own, high, child_rss))
                if own > 192 * 1024 or child_rss > 128 * 1024 or time.monotonic() - started > 30:
                    aborted.append('RSS or wall-clock guard exceeded')
                    for pid in children | {server.pid}:
                        try:
                            os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    return
                stop.wait(0.01)

        watcher = threading.Thread(target=monitor)
        watcher.start()
        try:
            while not Path(endpoint).exists():
                if server.poll() is not None or time.monotonic() - started > 5:
                    raise RuntimeError('daemon did not start')
                time.sleep(0.01)

            def call(revision):
                # Both streams reach the capture limit exactly, without truncation.
                script = ("python3 -c 'import sys,time,resource; sys.stdout.write(\"x\"*65536); "
                          "sys.stderr.write(\"y\"*65536); "
                          f"open(\"child-rss-{revision}\",\"w\").write(str(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)); "
                          "time.sleep(0.01)'")
                request = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                           'params': {'name': 'shell_run', 'arguments': {
                               'script': script, 'cwd': directory, 'timeout_ms': 2000,
                               'reuse': {'scope': 'synthetic', 'input_revision': str(revision), 'ttl_ms': 10000}}}}
                body = json.dumps(request).encode()
                header = f'POST /mcp HTTP/1.1\r\nHost: zrun\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode()
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(5)
                    client.connect(endpoint)
                    client.sendall(header + body)
                    response = bytearray()
                    while chunk := client.recv(65536):
                        response.extend(chunk)
                        if len(response) > 2 * 1024**2:
                            raise RuntimeError('bounded response limit exceeded')
                delivery = json.loads(response.split(b'\r\n\r\n', 1)[1])['result']['structuredContent']
                payload = delivery['result']
                assert payload['exit_code'] == 0 and not payload['truncated'], payload
                assert payload['stdout']['data'] == 'x' * 65536
                assert payload['stderr']['data'] == 'y' * 65536
                return delivery['disposition']

            baseline = memory(server.pid)[0]
            assert call(0) == 'executed'
            assert call(0) == 'cached'
            for revision in range(1, 120):
                assert call(revision) == 'executed'
            filled = memory(server.pid)[0]
            time.sleep(11.2)
            idle = memory(server.pid)[0]
            assert call(119) == 'executed'
            if aborted:
                raise RuntimeError(aborted[0])
            return {'binary': binary, 'baseline_rss_kib': baseline, 'filled_rss_kib': filled,
                    'idle_rss_kib': idle, 'peak_rss_kib': max(s[2] for s in samples),
                    'descendants_peak_sampled_rss_kib': max(s[3] for s in samples),
                    'command_child_peak_rss_kib': max(int(p.read_text()) for p in Path(directory).glob('child-rss-*')),
                    'semantics': 'byte-exact stdout/stderr, exit 0, no truncation, TTL hit then expiry',
                    'samples': samples}
        finally:
            stop.set()
            watcher.join()
            server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                for pid in descendants(server.pid) | {server.pid}:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                server.wait()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--repetitions', type=int, choices=range(1, 6), default=3)
    args = parser.parse_args()
    for _ in range(args.repetitions):
        print(json.dumps(probe(str(Path(args.binary).resolve()))), flush=True)
