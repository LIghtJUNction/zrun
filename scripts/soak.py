#!/usr/bin/env python3
"""Run one persistent P0 runtime for an explicitly selected duration.

Ctrl-C/SIGTERM terminates the owned child and preserves partial JSONL evidence.
The default does not start a 24-hour run: --duration-seconds is mandatory.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import statistics
import sys
import unittest

import benchmark as bench


def process_rss(pid):
    """Current resident bytes, never the historical high-water RSS."""
    if sys.platform.startswith("linux"):
        data = bench.read_text(f"/proc/{pid}/status", 32768)
        for line in (data or "").splitlines():
            if line.startswith("VmRSS:"):
                try:
                    return {"bytes": int(line.split()[1]) * 1024, "source": "proc_status_vmrss"}
                except (ValueError, IndexError):
                    break
        return {"bytes": None, "source": "proc_status_vmrss", "error": "RSS unavailable or process exited"}
    if sys.platform == "darwin":
        command = ["ps", "-o", "rss=", "-p", str(pid)]
        scale, source = 1024, "ps_rss"
    elif sys.platform == "win32":
        command = ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", f"(Get-Process -Id {pid}).WorkingSet64"]
        scale, source = 1, "powershell_working_set"
    else:
        return {"bytes": None, "source": "unsupported"}
    result = bench.command_output(command)
    try:
        if result.get("exit_code") == 0:
            return {"bytes": int(result["stdout"]) * scale, "source": source}
    except (ValueError, KeyError):
        pass
    return {"bytes": None, "source": source, "error": result}


def rss_trend(samples, warmup_seconds=0.0):
    valid = [(s["elapsed_secs"], s["rss"].get("bytes")) for s in samples
             if s["elapsed_secs"] >= warmup_seconds and bench.finite_number(s["rss"].get("bytes"))]
    result = {"samples": len(valid), "warmup_seconds": warmup_seconds,
              "slope_bytes_per_sec": None, "first_bytes": None, "last_bytes": None,
              "min_bytes": None, "max_bytes": None, "delta_bytes": None}
    if valid:
        xs, ys = zip(*valid)
        result.update(first_bytes=ys[0], last_bytes=ys[-1], min_bytes=min(ys), max_bytes=max(ys), delta_bytes=ys[-1] - ys[0])
        mx, my = statistics.mean(xs), statistics.mean(ys)
        denominator = sum((x - mx) ** 2 for x in xs)
        if denominator > 0:
            result["slope_bytes_per_sec"] = sum((x - mx) * (y - my) for x, y in valid) / denominator
    return result


def inspect_events(path):
    counts, finish, errors = {}, None, []
    try:
        with path.open(encoding="utf-8") as stream:
            for number, line in enumerate(stream, 1):
                if len(line) > 2 * 1024 * 1024:
                    errors.append(f"line {number}: event exceeds 2 MiB")
                    break
                try:
                    event = json.loads(line)
                    if not isinstance(event, dict):
                        raise ValueError("event is not an object")
                    kind = event.get("event", "error")
                    counts[kind] = counts.get(kind, 0) + 1
                    if kind == "finish":
                        finish = event
                    if event.get("status") == "error":
                        errors.append(f"line {number}: {event.get('error')}")
                except (ValueError, TypeError) as exc:
                    errors.append(f"line {number}: {exc}")
                if len(errors) >= 20:
                    break
    except (OSError, UnicodeError) as exc:
        errors.append(str(exc))
    return counts, finish, errors


def valid_finish(finish, args):
    if not isinstance(finish, dict):
        return False
    for key, expected in {"schema_version": 1, "event": "finish", "status": "ok", "engine": args.engine,
                          "scenario": "soak", "workers": 1, "tasks": args.tasks, "active_tasks": 0}.items():
        if finish.get(key) != expected:
            return False
    elapsed = finish.get("elapsed_secs")
    cycles = finish.get("cycles")
    config = finish.get("config", {})
    if not isinstance(config, dict):
        return False
    return (finish.get("correctness") is True and isinstance(cycles, int) and cycles > 0
            and finish.get("completed_tasks") == cycles * args.tasks
            and bench.finite_number(elapsed) and elapsed >= args.duration_seconds
            and config.get("duration_secs") == args.duration_seconds
            and config.get("yields") == args.yields
            and config.get("sample_interval_ms") == args.sample_seconds * 1000)


def duration_24h_covered(status, requested, finish_valid, actual):
    return status == "ok" and requested >= 86400 and finish_valid and bench.finite_number(actual) and actual >= 86400


def self_test():
    class SoakTests(unittest.TestCase):
        def test_trend(self):
            samples = [{"elapsed_secs": t, "rss": {"bytes": 100 + 5 * t}} for t in range(4)]
            self.assertEqual(rss_trend(samples)["slope_bytes_per_sec"], 5)
            self.assertEqual(rss_trend(samples, 2)["samples"], 2)
            self.assertIsNone(rss_trend([])["slope_bytes_per_sec"])
            self.assertIsNone(rss_trend([samples[0]])["slope_bytes_per_sec"])
        def test_duration_coverage(self):
            self.assertTrue(duration_24h_covered("ok", 86400, True, 86400.01))
            for values in [("cancelled", 86400, True, 86401), ("ok", 60, True, 60),
                           ("ok", 86400, False, 86401), ("ok", 86400, True, None),
                           ("ok", 86400, True, 86399)]:
                self.assertFalse(duration_24h_covered(*values))
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SoakTests))
    return 0 if result.wasSuccessful() else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path, help="new evidence directory outside Git")
    parser.add_argument("--duration-seconds", type=int, help="required: 86400 selects 24 hours explicitly")
    parser.add_argument("--engine", choices=("zrun", "tokio"), default="zrun")
    parser.add_argument("--tasks", type=int, choices=bench.TASK_SCALES, default=1000)
    parser.add_argument("--workers", type=int, choices=bench.WORKERS, default=1)
    parser.add_argument("--yields", type=int, default=16)
    parser.add_argument("--sample-seconds", type=int, default=5)
    parser.add_argument("--grace-seconds", type=int, default=30)
    parser.add_argument("--max-log-mib", type=int, default=256)
    parser.add_argument("--affinity")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.binary is None or args.output is None or args.duration_seconds is None:
        parser.error("--binary, --output and explicit --duration-seconds are required")
    if not 1 <= args.duration_seconds <= 604800 or not 1 <= args.sample_seconds <= 60:
        parser.error("duration must be 1..604800 s and sampling interval 1..60 s")
    if not 1 <= args.yields <= 10000 or not 1 <= args.grace_seconds <= 3600 or not 1 <= args.max_log_mib <= 4096:
        parser.error("yields, grace or log budget outside supported bounds")
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        parser.error("benchmark executable does not exist")
    if args.affinity:
        if not hasattr(os, "sched_setaffinity"):
            parser.error("setting affinity is unsupported on this platform")
        try:
            cpus = bench.parse_affinity(args.affinity)
            if not set(cpus).issubset(set(bench.affinity())):
                raise ValueError("requested CPUs are not all allowed")
            os.sched_setaffinity(0, cpus)
        except (ValueError, OSError) as exc:
            parser.error(str(exc))
    output = args.output.resolve()
    try:
        output.mkdir(parents=True, exist_ok=False)
    except OSError as exc:
        parser.error(str(exc))
    report = {"schema_version": 1, "status": "running", "environment": bench.environment(args.binary),
              "config": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "duration_24h_covered": False, "full_soak_gate": "not_evaluated"}
    if args.workers != 1:
        report.update(status="unsupported", reason="P0 current-thread worker=1 only", future_worker_matrix=list(bench.WORKERS))
        bench.atomic_json(output / "report.json", report)
        print(output / "report.json")
        return 2
    bench.install_signals()
    bench.atomic_json(output / "report.json", report)
    samples, next_sample = [], 0.0
    with (output / "controller-rss.jsonl").open("w", encoding="utf-8", buffering=1) as rss_file:
        def tick(pid, elapsed):
            nonlocal next_sample
            if elapsed >= next_sample:
                sample = {"elapsed_secs": elapsed, "pid": pid, "rss": process_rss(pid)}
                samples.append(sample)
                rss_file.write(json.dumps(sample, allow_nan=False) + "\n")
                next_sample = elapsed + args.sample_seconds
        command = [str(args.binary), "--engine", args.engine, "--scenario", "soak", "--tasks", str(args.tasks),
                   "--workers", "1", "--yields", str(args.yields), "--duration-secs", str(args.duration_seconds),
                   "--sample-interval-ms", str(args.sample_seconds * 1000)]
        process = bench.run_process(command, output / "runtime-events.jsonl", output / "runtime-stderr.txt",
                                    args.duration_seconds + args.grace_seconds, tick=tick,
                                    max_output_bytes=args.max_log_mib * 1024 * 1024)
    counts, finish, errors = inspect_events(output / "runtime-events.jsonl")
    valid = valid_finish(finish, args) and counts.get("finish") == 1 and counts.get("start") == 1 and not errors
    status = process["status"]
    if status == "ok" and not valid:
        status = "invalid_completion"
    report.update(status=status, process=process, event_counts=counts, finish=finish, event_errors=errors,
                  rss_trend=rss_trend(samples), rss_after_warmup=rss_trend(samples, min(60, args.duration_seconds / 4)))
    covered = duration_24h_covered(status, args.duration_seconds, valid, (finish or {}).get("elapsed_secs"))
    report["duration_24h_covered"] = covered
    report["full_soak_gate"] = "requires_RSS_and_scope_review" if covered else "not_evaluated"
    report["rss_note"] = "OLS slope is descriptive; allocator retention and page residency affect RSS. No leak-free verdict is inferred from slope."
    report["workload_scope"] = "Repeated spawn, self-wake/yield, join, exact task identities and zero live tasks after every batch. No TCP or timer/cancellation stress."
    bench.atomic_json(output / "report.json", report)
    print(output / "report.json")
    return 130 if status == "cancelled" else (0 if status == "ok" else 1)


if __name__ == "__main__":
    raise SystemExit(main())
