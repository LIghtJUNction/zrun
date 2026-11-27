# zrun 与 Tokio 的云端配对基准 · 2026-11-27

在 AMD EPYC 云机上，zrun 在单线程自唤醒和活跃任务内存两项取得确定优势。四 worker、一个外部提交线程时，Tokio 的任务吞吐仍高出约一个数量级。把生产者增加到四个后，两者吞吐区间重叠。TCP 的两档吞吐和尾延迟区间也都包含持平。结果分别按提交方式和运行时模式解释。

## 源码和实验条件

- 原始主线 `ab0b34844ad077db398a50fd89e08135ae9a8526`。A/B 基线 `68fb5520e6ef0342942b513598c81e2e5de48291` 只给该主线增加测量能力。最终实测运行时代码为 `c834f2956b5dcaac38d95a40a769634583fb8a49`，包版本 `0.1.0-p0.14`，Tokio 固定为 `1.54.1`。
- 云端 Linux 6.18.57，x86_64，AMD EPYC 9V74，来宾可见 9 个逻辑 CPU。调度测试两侧使用同一进程亲和集 CPU 0–7。TCP 服务使用 0–3，独立的双 worker 负载进程使用 4–5。云宿主的独占、频率和其他租户负载不可验证。
- Rust 1.100.0，编译器提交 `0ef1bc386059596dada8fb65b50425aab2ea637a`，LLVM 23.1.5，`x86_64-unknown-linux-gnu`。同一 release 可执行文件选择引擎，`opt-level=3`、`lto=false`、`debug=0`，未设置额外 RUSTFLAGS。Cargo.lock SHA-256 为 `4e0b7b0c0b7286c9f1a152b0834dad52dd86673be883b9212aab88d1612caac3`。
- 最终调度和 TCP 主实验每格 12 对独立进程，逐轮交换顺序。每次调度运行 100,000 个任务，每个 yield 任务自唤醒 16 次。每个延迟进程按固定索引采样 2,048 个任务。两边执行同一 Future，逐项验证任务标识、完成数和清空后的活动任务数。
- 单 worker 的普通调度负载使用调用线程驱动。多生产者用例的四个外部 OS 线程逐个 spawn，1 / 4 指后台 worker 数，根调用者与生产线程另计。计时从释放生产者到收齐全部结果。多 worker 的 yield 吞吐包括提交，单线程的 yield 计时从提交结束开始。zrun 设置显式容量，Tokio 在相同任务数下运行。
- 吞吐和内存表给出独立进程中位数。P50/P99 是每个进程先求分位数，再取进程中位数。比值是两组中位数之比。95% 区间按轮配对 bootstrap，主实验和 24 对复测各 5,000 次重采样。区间描述本机单格的不确定性，未对多格比较校正。

## 吞吐和 CPU 开销

下表比值均为 zrun / Tokio。吞吐越高越好，CPU 纳秒每唤醒越低越好。

| Worker | 工作量 | 单位 | zrun | Tokio | 比值 [95% 区间] |
|---:|---|---|---:|---:|---:|
| 1 | 单生产者 spawn + join | 百万任务/s | 3.815 | 3.18 | 1.200 [0.915, 1.483] |
| 1 | 每任务 16 次自唤醒 | 百万唤醒/s | 53.543 | 25.548 | 2.096 [1.963, 2.194] |
| 1 | 四生产线程同时 spawn | 百万任务/s | 1.767 | 1.689 | 1.046 [0.706, 1.250] |
| 1 | 自唤醒，进程 CPU | ns/次 | 18.642 | 39.132 | 0.476 [0.456, 0.509] |
| 1 | TCP 回环有效负载 | MB/s | 81.147 | 81.481 | 0.996 [0.870, 1.233] |
| 4 | 单生产者 spawn + join | 百万任务/s | 0.207 | 2.171 | 0.095 [0.075, 0.137] |
| 4 | 每任务 16 次自唤醒 | 百万唤醒/s | 4.069 | 49.079 | 0.083 [0.073, 0.115] |
| 4 | 四生产线程同时 spawn | 百万任务/s | 1.516 | 1.535 | 0.987 [0.576, 1.093] |
| 4 | 自唤醒，进程 CPU | ns/次 | 350.258 | 82.107 | 4.266 [3.285, 4.734] |
| 4 | TCP 回环有效负载 | MB/s | 86.797 | 84.001 | 1.033 [0.880, 1.154] |

## 操作延迟

单位为 µs。自唤醒从任务通知自身计到再次 poll，包含 zrun 有限次内联重 poll 的效果。跨线程用例从外部生产线程发出通知计到任务恢复。竞争提交从调用 spawn 前计到任务第一次 poll，包括生产者在提交调用里等待的时间。TCP RTT 从发送一帧计到读回并校验完整数据。单生产者 spawn/join 只测整批吞吐。

| Worker | 场景 | zrun P50 | zrun P99 | Tokio P50 | Tokio P99 | P99 比值 [95% 区间] |
|---:|---|---:|---:|---:|---:|---:|
| 1 | 自唤醒 | 0.03 | 0.04 | 0.05 | 1.758 | 0.023 [0.022, 0.023] |
| 1 | 外部线程唤醒 | 10,525.494 | 15,554.472 | 6,667.677 | 10,695.066 | 1.454 [1.080, 1.784] |
| 1 | 四生产者提交 | 18,747.427 | 30,523.177 | 20.053 | 241.858 | 126.203 [75.434, 254.337] |
| 1 | TCP RTT | 44.175 | 164.433 | 44.628 | 154.868 | 1.062 [0.727, 1.189] |
| 4 | 自唤醒 | 0.03 | 0.15 | 0.466 | 12.608 | 0.012 [0.008, 0.023] |
| 4 | 外部线程唤醒 | 3.099 | 115.868 | 9.803 | 173.166 | 0.669 [0.138, 2.990] |
| 4 | 四生产者提交 | 7.006 | 184.419 | 17.925 | 1,838.276 | 0.100 [0.047, 0.932] |
| 4 | TCP RTT | 43.373 | 118.47 | 42.651 | 134.389 | 0.882 [0.722, 1.172] |

TCP 使用 16 条长连接、256 字节载荷，每连接预热 8 次后测量 512 次。每帧检查连接号、序号和回显内容，吞吐按单向已验证字节计算。四生产者、单后台 worker 的 P99 是这次最严重的回退场景。该口径同时包含提交等待和排队至首次执行，不能仅凭延迟数值确定哪一把锁导致差距。

## 内存和取消

峰值取 Linux 内核的 VmHWM，在工作量结束时读数，包括进程、句柄和测量代码。TCP 只计服务进程。单位 MiB，每格显示 zrun / Tokio。等待任务行的每任务增量另以运行时创建后的 RSS 为基准。

| 工作量 | 1 worker 峰值 | 4 worker 峰值 |
|---|---:|---:|
| spawn + join | 15.55 / 26.92 | 14.05 / 27.04 |
| 16 次自唤醒 | 15.80 / 26.92 | 14.04 / 26.92 |
| 自唤醒延迟采样 | 18.55 / 39.17 | 17.05 / 39.05 |
| 跨线程唤醒 | 26.30 / 42.24 | 25.42 / 42.11 |
| 四线程提交 | 18.66 / 38.99 | 17.29 / 39.03 |
| 全部 pending 后取消 | 15.55 / 26.92 | 14.80 / 27.04 |
| TCP 服务进程 | 1.73 / 1.82 | 1.84 / 1.95 |

- 1 worker、100,000 个 pending 任务，每任务 RSS 增量为 144.79 / 264.07 字节。比值 0.548 [0.545, 0.548]。两边都确认任务首次 poll 后再逐个取消并等待取消结果。
- 4 worker、100,000 个 pending 任务，每任务 RSS 增量为 136.42 / 264.44 字节。比值 0.516 [0.515, 0.517]。两边都确认任务首次 poll 后再逐个取消并等待取消结果。
- 1 worker、百万 pending 任务的单次容量核验，双方各取消并回收 1,000,000 个任务，进程峰值 138.92 / 253.48 MiB。每侧只有一次，未用于估计性能比值。
- 4 worker、百万 pending 任务的单次容量核验，双方各取消并回收 1,000,000 个任务，进程峰值 132.92 / 253.54 MiB。每侧只有一次，未用于估计性能比值。

同一最终构建在四 worker 上分别连续运行 120 秒，每批 10,000 个任务、每任务自唤醒 16 次，逐批验证结果和活动任务归零。控制器约每秒采一次 RSS，前 30 秒为预热。

| 引擎 | 完成批数 | 完成任务 | 结束活动数 | 预热后 RSS 范围 MiB | 预热后首末差 KiB | 线性斜率 B/s |
|---|---:|---:|---:|---:|---:|---:|
| zrun | 4,801 | 48,010,000 | 0 | 3.406–3.441 | 36.0 | 308.1 |
| tokio | 34,272 | 342,720,000 | 0 | 4.527–4.527 | 0.0 | 0.0 |

120 秒的采样可以观察高频创建和释放后的短期驻留量。项目规定的 24 小时稳定性门槛仍待验证。

## 剖析、补丁和重复试验

原始主线用 gprofng 在云机采样。单线程、100,000 任务一批、16 次自唤醒、12 秒的采样中，通知停车线程、入队和取队首分别占独占样本 8.40%、7.56%、5.88%，共 119 个 1 ms 样本。四 worker、每任务一次自唤醒、15 秒的采样中，系统调用占 52.35%，取任务路径占 14.17%，登记表的完成回调占 7.53%，共 1,912 个样本。采样用来选择改动位置，不能把吞吐变化按这些百分比分摊。

调用线程从就绪队列一次取至多 16 项并保持 FIFO。剩余至多 15 个引用保存在 Runtime 上，根 Future 提前返回时仍可继续驱动，shutdown 会清理它们。当前正在运行的驱动重新发布自身任务时省去一次多余通知。普通后台池为外部逐项提交的生产线程选择队列，队列中已有超过 8 个就绪项时，下一次提交尝试相邻队列。已有停车通知和窃取路径继续处理并发唤醒。任务登记直接由持有的 Arc 创建弱引用。

下表比较最终运行时与相同测量代码的原始运行时。1 worker 为 16 对，4 worker 筛选为 12 对，之后对四 worker 的单生产者和竞争提交再独立复测 24 对。大于 1 表示候选数值变大。

| 对照 | 工作量 | 候选 / 原始 [95% 区间] |
|---|---|---:|
| 1 worker，16 对 | spawn/join 吞吐 | 1.015 [0.963, 1.068] |
| 1 worker，16 对 | 唤醒吞吐 | 1.103 [1.076, 1.150] |
| 1 worker，16 对 | CPU ns/唤醒 | 0.891 [0.861, 0.930] |
| 4 worker，12 对 | 竞争提交吞吐 | 1.261 [1.021, 1.520] |
| 4 worker，12 对 | 竞争提交 P99 | 1.552 [0.302, 2.324] |
| 4 worker，12 对 | spawn/join 吞吐 | 1.714 [1.107, 2.848] |
| 4 worker，12 对 | 唤醒吞吐 | 1.632 [1.089, 2.065] |
| 4 worker，12 对 | CPU ns/唤醒 | 0.693 [0.524, 0.973] |
| 4 worker，12 对 | 自唤醒 P99 | 0.866 [0.538, 1.389] |
| 4 worker，12 对 | 外部唤醒 P99 | 0.954 [0.150, 2.565] |
| 4 worker，24 对复测 | 竞争提交吞吐 | 1.200 [1.096, 1.374] |
| 4 worker，24 对复测 | 竞争提交 P99 | 0.826 [0.524, 5.406] |
| 4 worker，24 对复测 | spawn/join 吞吐 | 1.158 [1.055, 1.266] |
| 2 worker，10,000 任务，12 对 | 竞争提交吞吐 | 1.053 [0.938, 1.262] |
| 2 worker，10,000 任务，12 对 | 竞争提交 P99 | 1.127 [0.530, 1.317] |
| 2 worker，10,000 任务，12 对 | spawn/join 吞吐 | 1.471 [1.117, 2.048] |
| 8 worker，10,000 任务，12 对 | 竞争提交吞吐 | 1.281 [1.019, 1.542] |
| 8 worker，10,000 任务，12 对 | 竞争提交 P99 | 0.993 [0.674, 2.167] |
| 8 worker，10,000 任务，12 对 | spawn/join 吞吐 | 1.167 [0.982, 1.258] |

先前两项实验结果保留在原始证据中。短暂自旋后再停车，4 worker 的六个指标区间都跨过 1。将外部生产者永久绑定到固定队列，竞争提交吞吐提高到 2.633 倍 [1.783, 3.553]，代价是 P99 增至 5.155 倍 [3.304, 7.631]。最终选用可按队列积压轮换的实现，前述两个版本只用作实验对照。

2 和 8 worker 的 10,000 任务扩展核验中，2 worker 的单生产者吞吐改善，8 worker 的竞争提交吞吐改善。两档的竞争提交 P99 均未检出稳定变化。四 worker 的 24 对复测中该 P99 区间仍很宽，尚不能保证新调度策略能维持每种压力下的尾延迟。

## 正确性和适用范围

- 云端 Linux 的任务、执行器、I/O、时间、同步和根 crate 定向测试共 77 项通过，另显式执行 release 百万任务回归。新增取消回归连续 32 轮填满 32 个槽，等待定时器全部登记，验证超额拒绝，对每个句柄重复 abort，并核对取消结果、活动任务、就绪项和定时器清空。
- 提前结束根 Future 后，对已经预取的任务执行 abort、继续 join 和 shutdown，核对每个 Future 恰好析构一次。另验证一个外部生产者的首个任务占住 worker 时，另一个 worker 仍能执行其第二个任务。
- 两个 HTTP 客户端在 TTL 为 0 的配置下合并同一命令。统计确认只启动一次且存在一个合并调用，随后首个客户端断开，另一个客户端收到成功状态和完整输出。
- 运行 11 个现有 Loom 模型、工作区全目标严格 Clippy、rustfmt、统计脚本自测及全特性普通依赖树检查。Tokio 仅出现在对照程序。Miri、24 小时 soak、16 worker 和其他操作系统均未在本轮验收。完整多平台性能目标仍未满足。

## 复现

在单独用于测量的 Linux 云端检出中运行。CPU 编号需在主机允许的集合内。每项输出目录需为空且尚未创建。

```sh
git clone https://github.com/LIghtJUNction/zrun.git
cd zrun
git switch --detach c834f2956b5dcaac38d95a40a769634583fb8a49
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
cargo +1.100.0 build --locked --release -p zrun-bench
D=$(mktemp -d)
B=$PWD/target/release/zrun-bench
python3 scripts/benchmark.py --binary "$B" --output "$D/runtime" \
  --workers 1 4 --tasks 100000 --rounds 12 --bootstrap 5000 --affinity 0-7 \
  --latency-samples 2048 --scenarios spawn_join yield wake_latency \
  external_wake_latency contended_spawn pending_memory
python3 scripts/benchmark_tcp.py --binary target/release/zrun-tcp-bench \
  --output "$D/tcp" --workers 1 4 --client-workers 2 --server-affinity 0-3 \
  --client-affinity 4,5 --case 16:256:512 --rounds 12 --bootstrap 5000
for engine in zrun tokio; do
  python3 scripts/soak.py --binary "$B" --output "$D/soak-$engine" \
    --engine "$engine" --workers 4 --tasks 10000 --yields 16 \
    --duration-seconds 120 --sample-seconds 1 --affinity 0-7
done
python3 scripts/benchmark.py --binary "$B" --output "$D/million" \
  --workers 1 4 --tasks 1000000 --scenarios pending_memory --rounds 1 \
  --bootstrap 100 --affinity 0-7 --latency-samples 2048
```

要复现前后对照，依次在同一个干净检出编译两份二进制，保存各自的 Git 身份和 SHA-256，再运行随仓库提供的配对统计脚本。

```sh
for spec in base:68fb5520e6ef0342942b513598c81e2e5de48291 candidate:c834f2956b5dcaac38d95a40a769634583fb8a49; do
  name=${spec%%:*}
  revision=${spec#*:}
  git switch --detach "$revision" || exit 1
  test -z "$(git status --porcelain)" || exit 1
  cargo +1.100.0 build --locked --release -p zrun-bench || exit 1
  cp target/release/zrun-bench "$D/$name"
  python3 - "$D/$name" "$revision" <<'PYMETA'
import hashlib, json, pathlib, sys
p = pathlib.Path(sys.argv[1])
p.with_suffix(".json").write_text(json.dumps({
    "source_head": sys.argv[2], "source_status": "", "exit_code": 0,
    "binary_sha256": hashlib.sha256(p.read_bytes()).hexdigest()
}))
PYMETA
done
python3 scripts/compare_revisions.py --baseline "$D/base" --baseline-build "$D/base.json" \
  --candidate "$D/candidate" --candidate-build "$D/candidate.json" \
  --output "$D/confirm4" --workers 4 --tasks 100000 --scenarios spawn_join contended_spawn \
  --rounds 24 --bootstrap 5000 --seed 20261127 --affinity 0-7
```

性能采样与正式计时分开。原始主线在相同 release 配置下单独构建。以下为四 worker 的采样命令。单线程采样将 CPU 改为 1、workers 改为 1、yields 改为 16、duration 改为 12 秒。

```sh
git switch --detach ab0b34844ad077db398a50fd89e08135ae9a8526
cargo +1.100.0 build --locked --release -p zrun-bench --bin zrun-bench
ORIGINAL_BINARY=$PWD/target/release/zrun-bench
taskset -c 0-4 gprofng collect app -p 1 -o "$D/spawn-pool.er" \
  "$ORIGINAL_BINARY" --engine zrun --scenario soak --duration-secs 15 \
  --tasks 100000 --yields 1 --workers 4 --sample-interval-ms 1000
gprofng display text -functions "$D/spawn-pool.er"
```

主实验调度二进制 SHA-256 为 `3f1e768686837bb8bed7a5b00e3983fb7d38be68a64f37ad2a0857248a71f36b`，TCP 二进制为 `c76b13b637fe11416df07e7b3e8e538448f84a7c427535943a95b316806f374c`。这两个值用于识别实测文件。其他检出路径或构建环境可能产生不同的可执行文件。

原始进程输出、分位数样本、配对统计、采样摘要、内存时间序列和验证日志随本轮的 `zrun-cloud-evidence-20261127.zip` 交付。
