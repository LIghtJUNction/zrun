# Linux Future 槽与任务登记实验

对照基线 `f5824d8`。比较二进制经 Rust 1.100.0 release 构建，在 Intel Core i9-12900HX、Linux 7.3.5-arch1-1 上运行。除另有说明外，调度测试固定到逻辑 CPU 0–15，每格执行 10 对独立进程，先后顺序逐轮交替。区间是配对 bootstrap 95% 区间。

## Future 所有权与内存

`474bb0b` 由原子调度状态授予 Future 槽访问权。`OwnedPoll` 独占从槽中取出的 Future，消费该凭据才能归还或者完成。Pending 先写回，再用 release 转换发布；下一次 claim acquire 后才取出。关闭只有在成功取得非运行状态的槽并进入终态时才取 Future。运行中的取消由当前 poll 处理，用户回调和析构均在槽访问结束后执行。

1、2、4、8、16 worker 各测 10,000 和 100,000 tasks，共 1,200 个样本通过正确性校验。十格的等待任务 RSS / task 均下降约 5%–6%。百万等待任务的独立复测也得到相近差距。

| worker | 基线 B/task | 候选 B/task | 候选 / 基线 |
| ---: | ---: | ---: | ---: |
| 1 | 272.0 | 256.0 | 0.9412 [0.9412, 0.9413] |
| 8 | 264.0 | 248.0 | 0.9395 [0.9394, 0.9395] |

单 worker、100,000 任务唤醒吞吐比值为 1.183 [1.107, 1.291]，CPU / wake 为 0.844 [0.773, 0.909]。并行唤醒与 spawn/join 未测得同样稳定的收益。16 worker、10,000 任务的首轮自唤醒 P99 为 1.082 [1.008, 1.164]，同亲和再跑 20 对得到 1.021 [0.961, 1.107]。16 worker、100,000 任务的跨线程 P99 继续出现微秒和数十毫秒两组，单次区间不用于推断普遍优势。

## 登记表对照

`0f70bf2` 用容量预约在登记表锁外构造任务。并发容量测试通过。2、4、8、16 worker、100,000 任务的 320 个样本均通过正确性校验，spawn/join 吞吐没有稳定提高。16 worker 的 spawn/join 吞吐降为 0.956 [0.903, 0.979]，4 worker 的唤醒吞吐降为 0.932 [0.900, 0.972]。改动未进入主线。

## 对照诊断与正确性

在 CPU 16–17 上分别对两版做 2 ms CPU 采样。两版的 2 worker、百万任务、每任务 64 次唤醒均完成并核对校验和。`Task::poll_budget` 占独占采样 CPU 时间的比重从 43.23% 变为 33.16%。这是两趟采样的热点定位，整体收益以配对实验为准。

67 项工作区测试、格式、严格 Clippy、rustdoc 和 9 项 Loom 通过。新增 3 项 Loom 测试使用真实槽实现及 Loom 的 `UnsafeCell`，覆盖 Future 交接给下一 driver、Pending 和 shutdown 竞争，以及取消与关闭只能取走一次。Miri 使用 `nightly-2026-11-24` 执行 16 项执行器测试和 11 项任务测试，全部通过。

单 worker 和 8 worker 的百万任务校验通过。8 worker 在 30 秒短跑中完成 17,290,000 个任务，最后活跃数为 0。八 worker 的 Unix 千并发在 live 下执行 1,000 次，在 reuse 下执行 1 次并合并 999 次。官方 MCP SDK、真实 PTY shell、zguard、zplugin 和 zmem scope 均复验通过。启用所有 feature 时，核心与 CLI 的常规依赖树里没有 Tokio。此前候选的 24 小时长测仍在运行。

TCP 与上一版做了 240 个独立样本的配对测试。首轮 8 worker、单连接、64 B 的吞吐比值为 0.952 [0.892, 0.987]。同配置独立串行复测 20 对，吞吐为 0.993 [0.943, 1.028]，往返 P99 为 0.985 [0.815, 1.147]。首轮吞吐差距未复现。

首批 Tokio 对照、单连接复测和百万任务 Tokio 内存样本因等待脚本超时后仍启动后续测试而发生 CPU 重叠。该批排除，后面的 Tokio 结论全部来自重跑的串行样本。

## 串行对照 Tokio 1.54.1

同一源码 `474bb0b`，1、2、4、8、16 worker 各 120 个正确性通过的样本，每组 100,000 任务。表中为 zrun/Tokio 的中位数比值。

| worker | spawn/join 吞吐 | 唤醒吞吐 | CPU / wake | 跨线程 P99 | 等待内存 / task |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0.658 | 1.122 | 0.891 | 2.107 | 0.970 |
| 2 | 0.383 | 0.559 | 1.716 | 1.946 | 0.933 |
| 4 | 0.328 | 0.231 | 3.225 | 1.548 | 0.932 |
| 8 | 0.347 | 0.384 | 1.627 | 1.569 | 0.932 |
| 16 | 0.294 | 0.382 | 2.013 | 见复测 | 0.932 |

单 worker 的唤醒吞吐区间为 1.031–1.199，领先 Tokio，但尚未达到 1.2× 的普通门槛。完整验收继续按同一候选的各指标分别判断。

五档自唤醒 P99 和单 worker 空闲 RSS 均过线。16 worker 的跨线程 P99 在 CPU 0–15 内的区间上界为 0.898，但数据包含微秒与数十毫秒两组样本。CPU 0–17 的复测比值为 1.080 [0.406, 1.245]，跨线程领先未能在两种亲和范围内同时复现。百万等待任务对照 Tokio 的内存比值为单 worker 0.9696、8 worker 0.9387，仍高于 0.85 门槛。

TCP 回环的 240 个样本通过回显校验。服务端固定在 CPU 0–7，客户端固定在 CPU 8–15。24 个指标格中，以下四个往返 P99 用例过线，吞吐各格尚未过线。

| worker | 连接 × 帧长 | P99 比值 |
| ---: | --- | ---: |
| 1 | 32 × 64 KiB | 0.819 [0.654, 0.986] |
| 8 | 64 × 64 B | 0.613 [0.489, 0.769] |
| 8 | 128 × 4 KiB | 0.742 [0.665, 0.906] |
| 8 | 32 × 64 KiB | 0.807 [0.645, 0.991] |

原始数据与测试记录位于 Project Z 的 `dist`。

- `zrun-slot-paired-w{1,2,4,8,16}/report.json` 与 `zrun-slot-paired-w16-tail/report.json`
- `zrun-slot-memory-million-w{1,8}/report.json`
- `zrun-admission-paired-w{2,4,8,16}/report.json`
- `zrun-slot-w2-profile-{base,candidate}.er`
- `zrun-slot-tcp-paired/report.json` 与 `zrun-slot-tcp-w8-single-serial/report.json`
- `zrun-slot-tokio-serial-w{1,2,4,8,16}/report.json` 与 `zrun-slot-tokio-w16-affinity0-17/report.json`
- `zrun-slot-tokio-memory-million-serial/report.json` 与 `zrun-slot-tcp-tokio-serial/report.json`
- `zrun-slot-miri-all.log` 与 `zrun-slot-soak-short-w8/report.json`
