# P0 benchmark and soak protocol

This protocol measures the current-thread runtime implemented in [zrun](../src/lib.rs) against Tokio 1.54.1. It defines reproducible measurements and explicit acceptance states; adding the tools does not establish a speedup, million-task capacity, cross-platform pass, or 24-hour stability result. Project targets remain in [design.md](design.md) and work status in [plan.md](plan.md).

## 1. Scope and comparable work

The [comparator](../crates/zrun-bench/src/main.rs) uses identical Future implementations for both engines. Its generic adapters are compiled separately and do not box each task. Tokio is confined to the unpublished `zrun-bench` package; `zrun` remains the native runtime. Tokio uses `Builder::new_current_thread().enable_time()`. zrun uses `Runtime::with_max_tasks(tasks)`, including an explicit capacity of 1,000,000 for that scale. Runtime construction and JSON serialization are outside throughput timing.

The worker matrix is 1, 2, 4, 8, and 16. Task scales are 1,000, 10,000, 100,000, and 1,000,000. Unrequested cells remain untested. TCP uses a separate comparator.

Table 1. Work performed and the boundary of each measurement.

| Scenario | Timed or observed work | Interpretation |
| --- | --- | --- |
| `spawn_join` | Allocate the handle vector, spawn N tasks returning their index, then await every handle and verify its exact index. | End-to-end spawn and join throughput, including handle management. |
| `yield` | Pre-spawn N identical Futures. Each self-wakes and returns Pending K times, then returns its index. Time `block_on` through all joins. | N × K wake operations per second. Final polls and join bookkeeping are included. `yield_ns_per_op` is a wall-time proxy, not isolated CPU scheduling overhead. |
| `wake_latency` | Pre-spawn N Futures. First poll records time for a deterministic evenly spaced subset, self-wakes, and returns Pending. Second poll records elapsed time. Join every task. | Self-wake to next-poll latency under this workload. It does not measure an external producer wake, timer, socket readiness, or TCP latency. |
| `external_wake_latency` | Pre-spawn N Futures. Each sends its Waker to a dedicated producer thread during its initial poll. After all registrations arrive, the producer wakes each task once. A deterministic subset records time immediately before notification and again at resumption. | Cross-thread notification-to-poll latency under a burst, including queueing and worker handoff. |
| `pending_memory` | Record RSS after runtime creation. Spawn N Futures, confirm every one has been polled and is Pending, hold for 20 ms by default, and record live RSS. Abort and join all tasks. | Incremental resident bytes and bytes per pending task, including retained join handles, the shared counter and task Futures. It is not allocator-only task-header size. |
| `idle` | Record process RSS, construct the runtime with the requested task cap, execute an empty root Future, and record RSS again. | Runtime-construction RSS increment. Requested task capacity is configuration, not an assertion that N tasks were allocated. |

Every task-returning scenario verifies each handle's expected index, completed count, checksum and zero remaining live tasks. Pending memory verifies N initial polls and N cancellations. A failed check produces a nonzero process exit and cannot supply a valid measurement. The comparator defaults to 16 self-wakes per task and at most 10,000 latency observations per process. The selected latency task indices are deterministic and identical for both engines; the timestamp overhead is part of that measurement.

RSS means the current resident set, using Linux `/proc/<pid>/status` `VmRSS`, macOS `ps` RSS, or Windows process working set. Values are converted to bytes. The raw record includes the source and availability. Negative differences are preserved; missing, zero and negative values are not treated as a successful memory ratio. RSS has page-level granularity and includes allocator retention and shared-page residency, so it cannot by itself prove allocation size or a memory leak.

## 2. Build and collect independent samples

Build the release comparator before collecting evidence. On the shared Project Z development machine, the coordinator owns Cargo and the shared target lock described in the workspace `AGENTS.md`; workers must not start a competing Cargo process. The commands below illustrate a standalone checkout. Keep raw output outside the Git repository.

```sh
cargo build -p zrun-bench --release --locked
python3 scripts/benchmark.py \
  --binary target/release/zrun-bench \
  --output /var/tmp/zrun-bench-example \
  --tasks 1000 10000 100000 1000000 \
  --workers 1 2 4 8 16 \
  --rounds 10
```

The output directory must not already exist. Windows uses `python` and `target/release/zrun-bench.exe`; choose a writable output path for that machine. The runner requires Python 3.11 or later and only its standard library. It does not invoke Cargo, install dependencies, change CPU governors, or run the soak workload.

Each `(round, scale, scenario, engine)` launches a fresh executable process. Engine order is zrun then Tokio on even numbered rounds and Tokio then zrun on odd numbered rounds. A round pairs these two adjacent independent processes; it does not reuse a runtime or process across samples. There are no discarded warmup rounds or hidden retries. Cold-process and allocator startup effects therefore belong to the protocol. `--rounds 1` is a smoke check and cannot satisfy a performance target.

Use `--affinity 2` on Linux to restrict the runner and inherited child affinity to an allowed CPU. The runner records the effective set before each launch; a requested unavailable CPU or an unsupported affinity-setting platform is an error. Without the option, existing affinity is inherited and recorded. A single runtime worker does not imply a pinned CPU. Record governor, thermal state and other workloads when interpreting results; the runner captures available CPU/governor information and per-sample system load but does not claim that the host was otherwise idle.

Example for a short protocol smoke, without a performance claim:

```sh
python3 scripts/benchmark.py \
  --binary target/release/zrun-bench \
  --output /var/tmp/zrun-bench-smoke \
  --tasks 1000 --rounds 1 --bootstrap 100
```

Per-process timeout defaults to 120 seconds and is configurable with `--timeout`. Timeouts, invalid output, incorrect task counts, missing processes and cancellation remain in the report. Ctrl-C or SIGTERM stops the currently owned child and saves collected evidence. Raw stdout and stderr files are retained. A process-output budget prevents unbounded logging; exceeding it is an error.

## 3. Evidence and statistical decisions

`report.json` contains the full configuration, raw parsed samples, collection environment and summary. Each sample also has separate stdout JSON and stderr files. Environment fields include CPU model and logical count, OS/kernel, available memory/governor data, affinity, Python and on-path Rust versions, source HEAD/dirty state, Cargo.lock hash and comparator entries, executable path and SHA-256, and compiler flags. The executable itself reports the compiler used at build time, target, profile, optimization level and encoded Rust flags. Checkout identity describes collection-time sources; it is not proof that a dirty checkout exactly matches an older executable. Keep build logs when making a reproducibility claim.

Main result fields are `schema_version`, `engine`, `scenario`, `workers`, `tasks`, `status`, `config`, `build`, `correctness`, `completed_tasks`, `checksum`, `active_tasks`, `metrics`, `process_rss`, and `runtime_rss`. Latency records additionally retain `latency_samples_ns`; pending-memory records retain `pending_polled_tasks`, `cancelled_tasks`, `live_rss`, `after_cleanup_rss`, and `join_handle_bytes`. Unsupported and failed records carry their reason instead of invented measurements.

The [runner](../scripts/benchmark.py) reports median, P95 and P99 across independent process values using linear interpolation, plus a percentile-bootstrap 95% confidence interval for the median. Its default 5,000 bootstrap draws use a recorded seed and stable SHA-256-derived per-cell seeds. A bootstrap resample has the same number of process values as its original cell. These intervals describe sampling uncertainty in this experiment; they do not remove host noise or establish generality across hardware.

For a target, the ratio is **median(zrun) / median(Tokio)**. The paired bootstrap resamples round indices and recomputes both medians, preserving the round pairing. It is not the median of per-round ratios. The report shows a point-estimate decision separately from the acceptance decision. A normal or challenge target passes only when all requested pairs are present and strictly positive, there are at least 10 independent rounds, the executable reports a release profile, and the entire 95% ratio interval satisfies the correct threshold. There is no multiple-comparison adjustment; per-cell decisions are exploratory and do not imply a full project pass.

Table 2. P0 ratio directions and targets inherited from the design.

| Metric and unit | Better direction | Normal target | Challenge target |
| --- | --- | --- | --- |
| `spawn_join_ops_per_sec`, tasks/s | Higher | ≥ 1.20 | ≥ 2.00 |
| `wake_ops_per_sec`, wakes/s | Higher | ≥ 1.20 | ≥ 2.00 |
| `yield_cpu_ns_per_wake`, CPU ns/wake | Lower | ≤ 0.85 | ≤ 0.50 |
| `wake_p99_ns`, ns | Lower | < 1.00 | ≤ 0.70 |
| `external_wake_p99_ns`, ns | Lower | < 1.00 | ≤ 0.70 |
| `pending_bytes_per_task`, bytes/task | Lower | ≤ 0.85 | ≤ 0.60 |
| `idle_rss_delta_bytes`, bytes | Lower | ≤ 0.80 | ≤ 0.50 |

On Linux, `yield_process_cpu_ns` is the difference from `CLOCK_PROCESS_CPUTIME_ID` across the timed yield workload. It counts CPU consumed by the process's threads, including both user and kernel time. Division by the verified wake count gives CPU ns/wake. At one worker, it starts after submission. At multiple workers, it includes submission because workers can already be polling. Runtime creation, the post-run registry drain, and report output are outside the CPU interval. The timed code includes the common futures and join verification. This is a workload CPU cost, distinct from wall time and function-level attribution. Unsupported platforms return null.

For each wake-latency scenario, the per-process P99 is computed from the retained raw latencies. The runner verifies that value and summarizes those process P99 values; the summary's P99 is **not** a pooled P99 over all tasks and rounds. Low latency-sample counts and short runs should remain visible in any interpretation.

`missing_or_nonpositive_measurements`, `insufficient_independent_rounds`, `requires_release_build`, `not_requested` and `unsupported` never pass. Source/CI failures also remain separate from performance. The full project gate remains `not_evaluated` because this runner does not cover TCP or full correctness and stability. The TCP runner reports throughput > 1.00 (challenge ≥ 1.20), and TCP P99 < 1.00 (challenge ≤ 0.80). The full result requires the same source candidate and Tokio baseline to satisfy every required metric and configuration, plus correctness and stability evidence. Equality does not pass a strict threshold.

## 4. Explicit, cancellable soak duration

The [soak driver](../scripts/soak.py) keeps one runtime process alive, repeatedly spawning tasks, self-waking, joining them, validating task identities and checking zero live tasks after each batch. This stresses repeated task lifecycle and resident memory retention. It does not exercise TCP, timer cancellation storms, or multi-worker scheduling. Those need distinct workloads and evidence.

The duration argument is mandatory. A short smoke and an explicitly selected 24-hour run are different commands:

```sh
python3 scripts/soak.py --binary target/release/zrun-bench \
  --output /var/tmp/zrun-soak-smoke --duration-seconds 10

python3 scripts/soak.py --binary target/release/zrun-bench \
  --output /var/tmp/zrun-soak-24h --duration-seconds 86400 \
  --tasks 10000 --sample-seconds 5
```

`--engine tokio` runs the same lifecycle workload on the comparator baseline. Each command uses a new output directory. The driver records requested and observed duration, child exit status, runtime JSONL events, stderr, and independently sampled child RSS in `controller-rss.jsonl`. Controller sampling continues even if a runtime stops completing batches. A duration-plus-grace watchdog terminates a hung child. Ctrl-C and SIGTERM terminate only the child process owned by this driver and retain partial records; cancellation is never a completed soak.

The default interval is five seconds and default child-log budget is 256 MiB. `--sample-seconds`, `--grace-seconds` and `--max-log-mib` make those limits explicit. Missing RSS is recorded rather than replaced by zero. The report includes full and post-warmup ordinary-least-squares RSS slopes, first/last values and min/max. The warmup exclusion is the smaller of 60 seconds and one quarter of the requested duration. These are descriptive values, not automatic leak-free decisions.

`duration_24h_covered` becomes true only after a valid finish event, successful process exit, a request of at least 86,400 seconds and an observed runtime duration of at least 86,400 seconds. Even then, `full_soak_gate` requires RSS and workload-scope review. A short run, timeout, killed process or absent finish event cannot become 24-hour evidence. No long run is started by installing the tool or by its default arguments.

## 5. Source gates and evidence review

The [CI workflow](../.github/workflows/ci.yml) has only `workflow_dispatch`. It defines Linux, Windows and macOS source jobs for formatting, workspace tests, Clippy, Python statistical checks, a native dependency check that rejects Tokio under zrun, and a one-round comparator protocol smoke. It has no push/PR/schedule trigger, release step, registry upload or write permission. A workflow definition is not proof that these platforms have passed; retain each actual run's URL and logs.

Run the pure statistical checks without launching a runtime:

```sh
python3 scripts/benchmark.py --self-test
python3 scripts/soak.py --self-test
```

Table 3. Claims and the evidence needed before reporting them.

| Proposed claim | Required evidence | State from implementation alone |
| --- | --- | --- |
| P0 is faster or uses less resident memory on a particular configuration | Complete paired release samples, raw files, host/build identity, ratio interval and correct threshold direction | Needs measured results |
| One million tasks work | Actual 1,000,000-task records with completed accounting and no timeout/failure; identify which scenarios ran | Needs a real run |
| 24-hour lifecycle stability | Valid 24-hour finish, periodic runtime/controller RSS, no task-accounting errors, reviewed trend and scope | Needs a real run and review |
| Linux/Windows/macOS source gates pass | Successful manual workflow runs for all three systems | Needs CI execution |
| P1 or TCP targets pass | Implemented corresponding workload and matched baseline measurements | Unsupported in P0 |
| A scheduling mechanism causes an improvement | Controlled ablation with other settings held fixed | No causal claim from the comparator |

Before publishing conclusions, review five points: the precise contribution being claimed, reproducibility of the written protocol, strength of the measured effect, completeness of scales/platforms/workloads, and whether the metric actually tests the stated mechanism. Preserve failures and narrow claims to observed configurations. This review structure follows the user-requested [Research-Paper-Writing-Skills](https://github.com/Master-cai/Research-Paper-Writing-Skills), particularly its experiment and evidence-review guidance.

## 多 worker 候选

`--workers 1 2 4 8 16` 为每个规模分别运行配对样本。一个 worker 沿用两个运行时的 current-thread 入口。更多 worker 使用 zrun Pool 和相同线程数的 Tokio multi-thread runtime。

多线程运行时在提交期间可以执行任务。多 worker 的 yield 样本从提交前计时，包含提交及收集开销，原始记录标记 `yield_includes_submission`。单 worker 保留原来从提交后计时的口径。每个线程数下的 zrun/Tokio 样本使用相同口径。

## TCP 回环对照

每个样本分别启动一个服务进程和一个负载进程。服务使用 zrun 或 Tokio，负载进程使用同一个 Tokio 客户端。双方均开启 TCP_NODELAY，每条连接保持至用例结束。客户端完成连接和预热后，在同一个起点释放所有工作。每一帧都校验连接编号、序号和完整回显内容。服务端也校验预期连接和消息是否全部完成。

吞吐以单向已校验的 payload 字节除以整批计时区间计算。RTT 从客户端发送起算，直到完整回显校验完成。每条连接按固定步长保留延迟样本，上限按连接数分配。报告同时保存 P50、P95、P99、原始延迟和两个进程的 CPU tick。客户端 CPU tick 对应计时阶段，服务端 CPU tick 包括建立连接和退出。

每个配置运行至少 10 对独立样本，交替两侧顺序。以配对自举的 95% 区间判定吞吐是否严格高于 Tokio，以及 P99 是否严格低于 Tokio。查看 CPU 亲和性和客户端 CPU 使用量，判断负载生成器是否限制了吞吐。该比较覆盖回环 TCP，与外部网络、持续运行和其他平台的门禁分开。

```sh
cargo build --locked --release -p zrun-bench --bin zrun-tcp-bench
python3 scripts/benchmark_tcp.py --binary target/release/zrun-tcp-bench \
  --output artifacts/tcp-linux --workers 1 4 --client-workers 4 \
  --server-affinity 0-7 --client-affinity 8-15
```

同一版本的客户端也可用 `--baseline PATH` 对照较早版本的 zrun 服务端。报告此时按两版相等的阈值评估变化，与 Tokio 的阶段门槛分别解读。
