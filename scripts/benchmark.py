#!/usr/bin/env python3
"""P0 comparator orchestration; Python 3.11+, standard library only.

Every engine/scenario/scale/round is a fresh process. This script never builds
Rust, changes the system governor, or promotes incomplete evidence to a pass.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import operator
import os
from pathlib import Path
import platform
import random
import signal
import statistics
import subprocess
import sys
import time
import tomllib
import unittest

DEFAULT_SCENARIOS = ("spawn_join", "yield", "wake_latency", "external_wake_latency", "pending_memory", "idle")
SCENARIOS = (*DEFAULT_SCENARIOS, "batch_join")
TASK_SCALES = (1000, 10000, 100000, 1000000)
WORKERS = (1, 2, 4, 8, 16)
METRICS = {
    "batch_join_ops_per_sec": ("batch_join", "higher", 1.20, 2.00, "tasks/s"),
    "spawn_join_ops_per_sec": ("spawn_join", "higher", 1.20, 2.00, "tasks/s"),
    "wake_ops_per_sec": ("yield", "higher", 1.20, 2.00, "wakes/s"),
    "yield_cpu_ns_per_wake": ("yield", "lower", 0.85, 0.50, "CPU ns/wake"),
    "wake_p99_ns": ("wake_latency", "lower", 1.00, 0.70, "ns"),
    "external_wake_p99_ns": ("external_wake_latency", "lower", 1.00, 0.70, "ns"),
    "pending_bytes_per_task": ("pending_memory", "lower", 0.85, 0.60, "bytes/task"),
    "idle_rss_delta_bytes": ("idle", "lower", 0.80, 0.50, "bytes"),
}
STOP_REQUESTED = False


def finite_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def quantile(values, q):
    """Linear interpolation (R7), including exact endpoints."""
    if not values or not 0 <= q <= 1:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * q
    lo, hi = math.floor(position), math.ceil(position)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (position - lo)


def bootstrap_median(values, iterations, seed):
    if not values:
        return None
    rng = random.Random(seed)
    samples = [statistics.median(rng.choices(values, k=len(values))) for _ in range(iterations)]
    return [quantile(samples, 0.025), quantile(samples, 0.975)]


def describe(values, iterations=5000, seed=20261123):
    valid = [float(v) for v in values if finite_number(v)]
    return {"n": len(valid), "missing": len(values) - len(valid),
            "median": quantile(valid, 0.5), "p95": quantile(valid, 0.95),
            "p99": quantile(valid, 0.99),
            "median_bootstrap_95_ci": bootstrap_median(valid, iterations, seed)}


def compare_pairs(pairs, *, direction, normal, challenge, expected, iterations, seed, release,
                  strict_normal=False):
    """Pair by round; bootstrap the ratio of medians, never the median of ratios."""
    valid = [(float(z), float(t)) for z, t in pairs
             if finite_number(z) and finite_number(t) and z > 0 and t > 0]
    normal_operator, compare_normal = {
        ("higher", False): (">=", operator.ge),
        ("higher", True): (">", operator.gt),
        ("lower", False): ("<=", operator.le),
        ("lower", True): ("<", operator.lt),
    }[(direction, strict_normal)]
    result = {"ratio_definition": "median(zrun) / median(tokio)", "direction": direction,
              "normal_threshold": normal, "normal_operator": normal_operator, "challenge_threshold": challenge,
              "pairs": len(valid), "expected_pairs": expected, "ratio": None,
              "ratio_bootstrap_95_ci": None, "normal_pass": False, "challenge_pass": False}
    if valid:
        z, t = zip(*valid)
        ratio = statistics.median(z) / statistics.median(t)
        rng = random.Random(seed)
        ratios = []
        for _ in range(iterations):
            draw = rng.choices(valid, k=len(valid))
            ratios.append(statistics.median(a for a, _ in draw) / statistics.median(b for _, b in draw))
        ci = [quantile(ratios, 0.025), quantile(ratios, 0.975)]
        result.update(ratio=ratio, ratio_bootstrap_95_ci=ci)
        result["normal_point_pass"] = compare_normal(ratio, normal)
        result["challenge_point_pass"] = ratio >= challenge if direction == "higher" else ratio <= challenge
    if len(valid) != expected or len(pairs) != expected:
        result["status"] = "missing_or_nonpositive_measurements"
    elif expected < 10:
        result["status"] = "insufficient_independent_rounds"
    elif not release:
        result["status"] = "requires_release_build"
    else:
        result["status"] = "evaluated"
        edge = result["ratio_bootstrap_95_ci"][0 if direction == "higher" else 1]
        result["normal_pass"] = compare_normal(edge, normal)
        result["challenge_pass"] = edge >= challenge if direction == "higher" else edge <= challenge
    return result


def stable_seed(seed, *parts):
    suffix = hashlib.sha256("|".join(map(str, parts)).encode()).digest()[:8]
    return seed ^ int.from_bytes(suffix, "big")


def parse_affinity(text):
    cpus = set()
    for part in text.split(","):
        ends = part.split("-")
        if len(ends) > 2 or any(not v.isdigit() for v in ends):
            raise ValueError("affinity must look like 0,2-4")
        lo, hi = int(ends[0]), int(ends[-1])
        if lo > hi or hi > 65535:
            raise ValueError("invalid CPU range")
        cpus.update(range(lo, hi + 1))
    if not cpus:
        raise ValueError("empty CPU affinity")
    return sorted(cpus)


def affinity():
    return sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None


def command_output(command, cwd=None):
    try:
        p = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=10, check=False)
        return {"command": command, "exit_code": p.returncode,
                "stdout": p.stdout[:16384].strip(), "stderr": p.stderr[:4096].strip()}
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {"command": command, "error": str(exc)}


def read_text(path, limit=65536):
    try:
        with open(path, encoding="utf-8", errors="replace") as stream:
            return stream.read(limit)
    except OSError:
        return None


def environment(binary):
    repo = Path(__file__).resolve().parents[1]
    cpu_info = read_text("/proc/cpuinfo")
    model = next((s.split(":", 1)[1].strip() for s in (cpu_info or "").splitlines()
                  if s.startswith("model name")), platform.processor())
    lock = repo / "Cargo.lock"
    lock_info = None
    if lock.is_file():
        raw = lock.read_bytes()
        try:
            packages = tomllib.loads(raw.decode()).get("package", [])
            lock_info = {"sha256": hashlib.sha256(raw).hexdigest(),
                         "comparator_packages": [p for p in packages if p["name"] in ("tokio", "zrun", "zrun-bench")]}
        except (ValueError, UnicodeError) as exc:
            lock_info = {"error": str(exc)}
    digest = hashlib.sha256()
    with binary.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    cpu_detail = None
    if sys.platform == "darwin":
        cpu_detail = command_output(["sysctl", "-n", "machdep.cpu.brand_string", "hw.memsize"])
    elif sys.platform == "win32":
        cpu_detail = command_output(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                                     "Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors | ConvertTo-Json -Compress"])
    return {"captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "platform": platform.platform(), "kernel": platform.release(), "uname": dict(platform.uname()._asdict()),
            "cpu_model": model, "cpu_detail": cpu_detail, "logical_cpus": os.cpu_count(),
            "cpu_affinity": affinity(), "python": sys.version, "rustc_on_path": command_output(["rustc", "-Vv"]),
            "governor_cpu0": read_text("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor", 256),
            "memory": read_text("/proc/meminfo", 8192), "rustflags": os.environ.get("RUSTFLAGS"),
            "cargo_encoded_rustflags": os.environ.get("CARGO_ENCODED_RUSTFLAGS"),
            "binary": str(binary), "binary_sha256": digest.hexdigest(), "cargo_lock": lock_info,
            "source_head": command_output(["git", "rev-parse", "HEAD"], repo),
            "source_dirty": command_output(["git", "status", "--porcelain"], repo),
            "source_identity_note": "HEAD/dirty describe collection checkout; build metadata and binary hash identify executable."}


def atomic_json(path, value):
    temp = path.with_suffix(path.suffix + ".tmp")
    with temp.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")
    os.replace(temp, path)


def request_stop(_signum, _frame):
    global STOP_REQUESTED
    STOP_REQUESTED = True


def install_signals():
    signal.signal(signal.SIGINT, request_stop)
    signal.signal(signal.SIGTERM, request_stop)


def stop_process(process):
    if process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        else:
            process.kill()
        process.wait(timeout=5)
    except ProcessLookupError:
        pass


def run_process(command, stdout_path, stderr_path, timeout, tick=None, max_output_bytes=16 * 1024 * 1024):
    started = time.monotonic()
    record = {"command": command, "stdout": stdout_path.name, "stderr": stderr_path.name,
              "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
              "cpu_affinity": affinity()}
    process = None
    try:
        with stdout_path.open("wb") as out, stderr_path.open("wb") as err:
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                                       start_new_session=os.name == "posix")
            record["pid"] = process.pid
            while process.poll() is None:
                elapsed = time.monotonic() - started
                if STOP_REQUESTED:
                    record["status"] = "cancelled"
                    break
                if elapsed > timeout:
                    record["status"] = "timeout"
                    break
                if stdout_path.stat().st_size + stderr_path.stat().st_size > max_output_bytes:
                    record["status"] = "output_limit"
                    break
                if tick:
                    tick(process.pid, elapsed)
                time.sleep(0.05)
            if process.poll() is None:
                stop_process(process)
            record["exit_code"] = process.wait()
            record.setdefault("status", "ok" if process.returncode == 0 else "process_error")
    except (OSError, subprocess.SubprocessError) as exc:
        record.update(status="launch_error", error=str(exc))
    finally:
        if process is not None and process.poll() is None:
            stop_process(process)
        record["wall_elapsed_secs"] = time.monotonic() - started
    return record


def validate_sample(raw, engine, scenario, tasks, workers, yields, latency_samples, hold_ms):
    if not isinstance(raw, dict):
        return "result must be an object"
    for key, expected in {"schema_version": 1, "engine": engine, "scenario": scenario,
                          "tasks": tasks, "workers": workers, "status": "ok", "correctness": True,
                          "active_tasks": 0, "completed_tasks": 0 if scenario == "idle" else tasks}.items():
        if raw.get(key) != expected or (key == "correctness" and raw.get(key) is not True):
            return f"unexpected {key}: {raw.get(key)!r}"
    config = raw.get("config")
    if not isinstance(config, dict) or not isinstance(raw.get("build"), dict):
        return "config and build must be objects"
    for key, expected in {"yields": yields, "latency_samples": latency_samples, "hold_ms": hold_ms}.items():
        if config.get(key) != expected:
            return f"configuration mismatch: {key}"
    if scenario in ("spawn_join", "batch_join", "yield", "wake_latency", "external_wake_latency") and raw.get("checksum") != tasks * (tasks - 1) // 2:
        return "checksum mismatch"
    if scenario == "pending_memory" and (raw.get("pending_polled_tasks") != tasks or raw.get("cancelled_tasks") != tasks):
        return "pending task accounting mismatch"
    if not isinstance(raw.get("metrics"), dict):
        return "missing metrics object"
    if scenario == "batch_join":
        metrics = raw["metrics"]
        elapsed = metrics.get("elapsed_ns")
        submission = metrics.get("submission_ns")
        throughput = metrics.get("batch_join_ops_per_sec")
        if (not finite_number(elapsed) or elapsed <= 0
            or not finite_number(submission) or not 0 < submission <= elapsed
            or not finite_number(throughput)
            or not math.isclose(throughput, tasks * 1e9 / elapsed, rel_tol=1e-12)):
            return "invalid batch submission/completion timing"
    if scenario == "yield":
        metrics = raw["metrics"]
        cpu = metrics.get("yield_process_cpu_ns")
        per_wake = metrics.get("yield_cpu_ns_per_wake")
        if (cpu is not None or per_wake is not None) and (
            not finite_number(cpu) or cpu <= 0 or not finite_number(per_wake)
            or metrics.get("wake_ops") != tasks * yields
            or not math.isclose(per_wake, cpu / (tasks * yields), rel_tol=1e-12)
        ):
            return "invalid CPU measurement or wake count"
    if scenario in ("wake_latency", "external_wake_latency"):
        values = raw.get("latency_samples_ns")
        if not isinstance(values, list) or len(values) != min(tasks, latency_samples) or not all(finite_number(v) and v >= 0 for v in values):
            return "invalid raw latency samples"
        expected = quantile(values, 0.99)
        metric = "wake_p99_ns" if scenario == "wake_latency" else "external_wake_p99_ns"
        actual = raw["metrics"].get(metric)
        if not finite_number(actual) or not math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-6):
            return "P99 disagrees with raw latency samples"
    return None


def collect_sample(args, output, round_index, tasks, scenario, engine, *, workers=1):
    prefix = f"r{round_index:03d}-w{workers}-t{tasks}-{scenario}-{engine}"
    command = [str(args.binary), "--engine", engine, "--scenario", scenario, "--tasks", str(tasks),
               "--workers", str(workers), "--yields", str(args.yields), "--latency-samples", str(args.latency_samples),
               "--hold-ms", str(args.hold_ms)]
    record = run_process(command, output / f"{prefix}.stdout.json", output / f"{prefix}.stderr.txt", args.timeout)
    record.update(round=round_index, tasks=tasks, workers=workers, scenario=scenario, engine=engine)
    if record["status"] == "ok":
        try:
            raw = json.loads((output / record["stdout"]).read_text(encoding="utf-8"))
            error = validate_sample(raw, engine, scenario, tasks, workers, args.yields, args.latency_samples, args.hold_ms)
            record["sample"] = raw
            if error:
                record.update(status="invalid_sample", error=error)
        except (ValueError, OSError, UnicodeError) as exc:
            record.update(status="invalid_json", error=str(exc))
    return record


def summarize(records, args):
    cells = []
    for workers in args.workers:
        for tasks in args.tasks:
            for name, (scenario, direction, normal, challenge, unit) in METRICS.items():
                row = {"workers": workers, "tasks": tasks, "metric": name, "scenario": scenario, "unit": unit}
                if scenario not in args.scenarios:
                    row.update(status="not_requested",
                               normal_pass=False, challenge_pass=False)
                    cells.append(row)
                    continue
                found = {(r["round"], r["engine"]): r for r in records
                         if r["tasks"] == tasks and r["workers"] == workers and r["scenario"] == scenario and r["status"] == "ok"}
                values = {e: [found.get((i, e), {}).get("sample", {}).get("metrics", {}).get(name)
                              for i in range(args.rounds)] for e in ("zrun", "tokio")}
                seed = stable_seed(args.seed, workers, tasks, name)
                row["engines"] = {e: describe(v, args.bootstrap, stable_seed(seed, e)) for e, v in values.items()}
                release = all(found.get((i, e), {}).get("sample", {}).get("build", {}).get("profile") == "release"
                              for i in range(args.rounds) for e in ("zrun", "tokio"))
                row.update(compare_pairs(list(zip(values["zrun"], values["tokio"])), direction=direction,
                                         normal=normal, challenge=challenge, expected=args.rounds,
                                         iterations=args.bootstrap, seed=seed, release=release,
                                         strict_normal=normal == 1.0))
                cells.append(row)
    return {"metrics": cells, "full_project_gate": "not_evaluated",
            "unsupported": [{"metric": "tcp_throughput", "reason": "use scripts/benchmark_tcp.py for the separate TCP comparator", "normal_ratio_min": 1.00, "normal_operator": ">", "challenge_ratio_min": 1.20},
                            {"metric": "tcp_p99", "reason": "use scripts/benchmark_tcp.py for the separate TCP comparator", "normal_ratio_max": 1.00, "normal_operator": "<", "challenge_ratio_max": 0.80}],
            "latency_note": "Self-wake and external-wake P99 are summarized separately across independent processes.",
            "decision_rule": "At least 10 complete positive pairs, release binary, and entire paired-bootstrap 95% CI beyond threshold. No multiplicity correction; exploratory per-cell decisions."}


def self_test():
    class StatisticsTests(unittest.TestCase):
        def test_quantiles_and_missing(self):
            self.assertEqual(quantile([1, 2, 3, 4], 0.5), 2.5)
            self.assertAlmostEqual(quantile([0, 100], 0.99), 99)
            self.assertIsNone(quantile([], 0.5))
            stats = describe([1, None, float("nan"), 3], 100, 7)
            self.assertEqual((stats["n"], stats["missing"], stats["median"]), (2, 2, 2.0))
        def test_deterministic_interval(self):
            self.assertEqual(bootstrap_median([1, 3, 9], 200, 42), bootstrap_median([1, 3, 9], 200, 42))
        def test_metric_directions(self):
            for direction, pairs, threshold in [("higher", [(2, 1)] * 10, 1.2), ("lower", [(0.5, 1)] * 10, 0.8)]:
                result = compare_pairs(pairs, direction=direction, normal=threshold, challenge=threshold,
                                       expected=10, iterations=100, seed=4, release=True)
                self.assertTrue(result["normal_pass"])
                self.assertEqual(result["ratio_bootstrap_95_ci"], [pairs[0][0], pairs[0][0]])
        def test_strict_superiority_rejects_equal_ratios(self):
            for direction, winning in (("higher", 1.01), ("lower", 0.99)):
                for ratio, should_pass in ((1.0, False), (winning, True)):
                    result = compare_pairs([(ratio, 1.0)] * 10, direction=direction,
                                           normal=1.0, challenge=1.0, expected=10,
                                           iterations=100, seed=7, release=True, strict_normal=True)
                    self.assertEqual(result["normal_point_pass"], should_pass)
                    self.assertEqual(result["normal_pass"], should_pass)
        def test_incomplete_and_zero_never_pass(self):
            for pairs, n, release in [([(2, 1)] * 9, 10, True), ([(2, 1)] * 2, 2, True),
                                      ([(2, 0)] * 10, 10, True), ([(0, 1)] * 10, 10, True),
                                      ([(2, 1)] * 10, 10, False), ([(None, 1)] * 10, 10, True)]:
                result = compare_pairs(pairs, direction="higher", normal=1.2, challenge=2,
                                       expected=n, iterations=100, seed=4, release=release)
                self.assertFalse(result["normal_pass"])
        def test_affinity(self):
            self.assertEqual(parse_affinity("0,2-4,3"), [0, 2, 3, 4])
            for invalid in ("", "3-1", "a", "1-2-3", "0-999999"):
                with self.assertRaises(ValueError):
                    parse_affinity(invalid)
        def test_raw_latency_validation(self):
            for scenario, metric in (("wake_latency", "wake_p99_ns"),
                                     ("external_wake_latency", "external_wake_p99_ns")):
                sample = {"schema_version": 1, "engine": "zrun", "scenario": scenario, "tasks": 2,
                          "workers": 1, "status": "ok", "correctness": True, "active_tasks": 0,
                          "completed_tasks": 2, "checksum": 1, "config": {"yields": 16, "latency_samples": 10, "hold_ms": 20},
                          "build": {"profile": "release"}, "latency_samples_ns": [100, 200], "metrics": {metric: 199.0}}
                args = ("zrun", scenario, 2, 1, 16, 10, 20)
                self.assertIsNone(validate_sample(sample, *args))
                sample["metrics"][metric] = 10
                self.assertIsNotNone(validate_sample(sample, *args))
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(StatisticsTests))
    return 0 if result.wasSuccessful() else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path, help="new directory for raw data and report; use a path outside Git")
    parser.add_argument("--tasks", type=int, nargs="+", choices=TASK_SCALES, default=[1000])
    parser.add_argument("--workers", type=int, nargs="+", choices=WORKERS, default=[1])
    parser.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=list(DEFAULT_SCENARIOS))
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--bootstrap", type=int, default=5000)
    parser.add_argument("--seed", type=int, default=20261123)
    parser.add_argument("--yields", type=int, default=16)
    parser.add_argument("--latency-samples", type=int, default=10000)
    parser.add_argument("--hold-ms", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--affinity", help="Linux CPU IDs, e.g. 2 or 2-3; inherited by every child")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if args.binary is None or args.output is None:
        parser.error("--binary and --output are required")
    if not 1 <= args.rounds <= 1000 or not 100 <= args.bootstrap <= 100000:
        parser.error("rounds must be 1..1000 and bootstrap 100..100000")
    if not (0 if set(args.scenarios) == {"batch_join"} else 1) <= args.yields <= 10000 or not 1 <= args.latency_samples <= 100000 or not 0 <= args.hold_ms <= 10000:
        parser.error("scenario arguments outside supported bounds")
    if not math.isfinite(args.timeout) or not 0 < args.timeout <= 86400:
        parser.error("timeout must be finite and in (0,86400]")
    args.tasks, args.workers, args.scenarios = [list(dict.fromkeys(x)) for x in (args.tasks, args.workers, args.scenarios)]
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        parser.error("benchmark executable does not exist")
    if args.affinity:
        if not hasattr(os, "sched_setaffinity"):
            parser.error("setting affinity is unsupported on this platform")
        try:
            cpus = parse_affinity(args.affinity)
            if not set(cpus).issubset(set(affinity())):
                raise ValueError("requested CPUs are not all in the allowed affinity")
            os.sched_setaffinity(0, cpus)
        except (ValueError, OSError) as exc:
            parser.error(str(exc))
    output = args.output.resolve()
    try:
        output.mkdir(parents=True, exist_ok=False)
    except OSError as exc:
        parser.error(str(exc))
    install_signals()
    report = {"schema_version": 1, "status": "running", "environment": environment(args.binary),
              "config": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "samples": [], "future_matrix": {"workers": list(WORKERS), "tasks": list(TASK_SCALES)}}
    atomic_json(output / "report.json", report)
    for i in range(args.rounds):
        for workers in args.workers:
            for tasks in args.tasks:
                for scenario in args.scenarios:
                    if STOP_REQUESTED:
                        break
                    for engine in (("zrun", "tokio") if i % 2 == 0 else ("tokio", "zrun")):
                        if STOP_REQUESTED:
                            break
                        sample = collect_sample(args, output, i, tasks, scenario, engine, workers=workers)
                        report["samples"].append(sample)
                        atomic_json(output / "report.json", report)
                        print(f"round={i + 1}/{args.rounds} workers={workers} tasks={tasks} scenario={scenario} engine={engine} status={sample['status']}", flush=True)
    report["status"] = "cancelled" if STOP_REQUESTED else "completed"
    if not STOP_REQUESTED and any(s["status"] != "ok" for s in report["samples"]):
        report["status"] = "completed_with_errors"
    report["summary"] = summarize(report["samples"], args)
    atomic_json(output / "report.json", report)
    print(output / "report.json")
    return 130 if STOP_REQUESTED else int(report["status"] == "completed_with_errors")


if __name__ == "__main__":
    raise SystemExit(main())
