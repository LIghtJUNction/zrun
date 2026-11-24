# Linux worker 本地续跑队列

对照基线 `1343e48`。所有比较二进制由 Rust 1.100.0 以 release 构建，运行在 Intel Core i9-12900HX、Linux 7.3.5-arch1-1。调度基准默认固定到逻辑 CPU 0–15。每格用 10 对独立进程逐轮交替运行，区间为配对 bootstrap 95% 区间。

当前候选 `cb264a5` 每个 worker 最多持有四个从本 worker 的 poll 结束后交还的 runnable。它们由工作线程独占并按 FIFO 取出，无需共享队列互斥锁。最多连续优先处理三次本地任务，随后检查共享队列；本地队列满时回到原来的共享发布路径。新任务和空闲任务的外部唤醒仍走共享队列及原有休眠握手。关闭时通过任务登记表请求取消，worker 退出时释放队列引用。`stats()` 包含本地队列的深度快照。

## 方案筛选

单个本地暂存位 `77e93cd` 在 8 worker、100,000 tasks 下将唤醒吞吐提高至 1.156 [1.013, 1.198]，进程 CPU / wake 为 0.723 [0.653, 0.799]。2 worker 的自唤醒 P99 从 60 ns 变为 93 ns。另做两次各 20 对的复测。在 CPU 0–15 的配置下，P99 比值为 1.266 [1.091, 1.401]，相当于中位数从 96.0 ns 变为 121.5 ns。每个物理 P core 只选一个逻辑 CPU 时，区间跨过 1。

可窃取的 Crossbeam 本地队列 `4705582` 在 8 worker、10,000 tasks 下，跨线程 P99 为 1.164 [1.022, 1.329]，每个等待任务的 RSS 增量为 1.017 [1.016, 1.022]。8 worker、100,000 tasks 的 CPU 收益小于单个暂存位。该依赖未进入当前候选。

## 相对上一版

2、4、8、16 worker、10,000 和 100,000 任务，共 960 个独立进程样本通过校验。100,000 任务时的候选/基线比值如下。

| worker | 唤醒吞吐 | CPU / wake |
| ---: | ---: | ---: |
| 2 | 1.074 [1.026, 1.180] | 0.834 [0.753, 0.906] |
| 4 | 1.046 [0.945, 1.098] | 0.854 [0.780, 0.942] |
| 8 | 1.140 [0.924, 1.213] | 0.713 [0.646, 0.879] |
| 16 | 1.048 [0.835, 1.132] | 0.834 [0.764, 0.930] |

10,000 任务时四档的 CPU / wake 也下降，2、8 worker 的唤醒吞吐区间在 1 之上。两种 P99 的各格置信区间均跨过 1。16 worker 的跨线程 P99 仍出现微秒和数十毫秒两组值。

在 CPU 16–17 上用 gprofng 做两趟独立的 2 ms CPU 采样。两版都完成 2 worker、百万任务、每任务 64 次唤醒，校验和正确。基线中共享 `publish` 和 `take` 占 36.35% 的采样 CPU 时间。候选的共享 `publish` 和同时执行本地与共享取出的 `next_task` 合计占 11.43%。`Task::poll_budget` 在候选中占 45.62% 的独占采样时间，指令采样热点包含任务状态转换和 Future 槽的互斥锁。单次采样用于定位，收益依据上方配对实验判断。

另对候选的 2 worker、百万任务 spawn/join 做了单次采样。`Mutex::lock_contended` 占 19.84% 独占采样时间，`SharedPool::finished` 及其子调用占 33.27%。共享登记表的竞争仍落在任务回收路径，单靠本地续跑队列无法消除。

TCP 与上一版的直接配对包含 240 个样本、1/4/8 worker 和四种连接及帧长。所有样本通过回显校验，24 格的吞吐及往返 P99 区间均包含 1。

## 同一源码对 Tokio 1.54.1

以 `cb264a5` 对照 1、2、4、8、16 worker 和 100,000 任务，600 个样本通过。表中是 zrun/Tokio 的中位数比值。

| worker | spawn/join 吞吐 | 唤醒吞吐 | CPU / wake | 跨线程 P99 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 0.628 | 1.026 | 0.974 | 2.568 |
| 2 | 0.364 | 0.530 | 1.830 | 1.966 |
| 4 | 0.308 | 0.233 | 3.002 | 1.536 |
| 8 | 0.327 | 0.414 | 1.586 | 1.532 |
| 16 | 0.281 | 0.378 | 2.051 | 0.666 |

CPU 0–15 下的 35 格中，自唤醒 P99 五格与单 worker 空闲增量 RSS 共六格通过普通门槛。16 worker 的跨线程 P99 在此范围的区间上界为 1.130，仍未过线。改用 CPU 0–17 的复测比值为 0.500 [0.284, 0.934]，领先只在该亲和范围获得支持。每任务内存、spawn/join、唤醒吞吐和 CPU / wake 仍未达到普通门槛。

相同源码的 TCP 回环有 240 个正确性通过的样本。24 格中，8 worker 的三组往返 P99 通过领先门槛。

| 连接 × 帧长 | P99 比值 |
| --- | ---: |
| 64 × 64 B | 0.593 [0.482, 0.696] |
| 128 × 4 KiB | 0.666 [0.455, 0.900] |
| 32 × 64 KiB | 0.727 [0.558, 0.857] |

服务端固定在 CPU 0–7，客户端固定在 CPU 8–15。TCP 吞吐各格均未通过。直接修订对照未得到稳定差异，不能把与 Tokio 的这三格差距归因于本轮改动。

## 正确性与互操作

通过 67 项常规工作区测试、格式检查、严格 Clippy、rustdoc 和 6 项 Loom 模型。新增测试在另一项任务占用 worker 时检查本地 runnable 仍计入队列统计，随后显式取消并排空。单 worker 的显式百万任务测试及 8 worker 的百万任务独立样本均通过。8 worker 的 30 秒连续运行完成 18,080,000 项任务，最终活跃任务为 0。前一版 `77e642c` 的 4 worker 24 小时长测仍在进行。

常规依赖树检查覆盖启用所有 feature 的核心与 CLI，Tokio 只用于比较器。真实 PTY 中 `zrun` 进入已配置 shell。官方 MCP SDK 通过 Unix、TCP 和 stdio 转发。8 worker 的 Unix 千并发在 `live` 下执行 1,000 次，在 `reuse` 下执行 1 次、合并 999 次。zguard 缓存前准入测试通过，zplugin 示例验证了允许和拒绝，受管命令实际在 zmem scope 内运行。

原始数据位于 Project Z 的 `dist`。

- `zrun-private-ready-w{2,4,8,16}/report.json` 与 `zrun-private-ready-w2-tail-{siblings,physical}/report.json`
- `zrun-stealable-ready-w{4,8}/report.json`
- `zrun-ready-fifo-w{2,4,8,16}/report.json`
- `zrun-ready-fifo-w2-profile-{base,candidate}.er` 与 `zrun-ready-fifo-w2-spawn-profile.er`
- `zrun-ready-fifo-tokio/report.json` 与 `zrun-ready-fifo-tokio-w16-affinity0-17/report.json`
- `zrun-ready-fifo-tcp-{pairs,tokio}/report.json`
- `zrun-ready-fifo-soak-short-w8/report.json`
