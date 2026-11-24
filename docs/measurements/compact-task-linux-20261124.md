# Linux 任务、Future 与结果合并分配

对照 `5659cb5` 与验证树 `afd946c`，后者的本地测试提交为 `56377da`。Rust 1.100.0 release，Intel Core i9-12900HX、Linux 7.3.5-arch1-1。调度样本排定在 CPU 0–15 串行运行。每格用 10 对独立进程，逐轮交替先后顺序。区间为配对 bootstrap 95% 区间。Miri 使用 `nightly-2026-11-24`。

## 分配与所有权

类型擦除的 `TaskRef` 仍只占一个指针。大小及对齐均不超过 128 B 的 Future 与任务头、结果槽合并为一次分配。其他 Future 固定在单独的 Box，终止时立即释放这块内存。调度状态授予固定地址 Future 的唯一 poll 和析构权。执行引用和 join 共享分配，最后一个执行引用在析构和结算期间持有收尾位。只有收尾位与 join 位都释放时才回收内存。完成前的 poll waker 借用当前执行引用；Future 保存的 waker 从 clone 获得独立引用。

核对 Rust `.await` 降低后的行为时发现，原实现会在 Future 析构成功后才发布结果。修正版保留该顺序，并在用户析构前清除 Future 的存活标记。专门的回归断言，Ready 后的析构 panic 由 JoinHandle 得到一次明确错误，Future 只析构一次。下面的验收数字来自修正版二进制。

## 相对上一版

五档 worker 各 120 个样本，共 600 个正确性通过的独立进程，均为 100,000 任务。

| worker | spawn/join 吞吐 | 唤醒吞吐 | CPU / wake | 等待内存 / task |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1.317 [1.287, 1.480] | 1.236 [1.134, 1.301] | 0.816 [0.803, 0.882] | 0.563 |
| 2 | 1.812 [1.677, 1.915] | 1.472 [1.412, 1.597] | 0.792 [0.724, 0.822] | 0.549 |
| 4 | 1.411 [1.216, 1.455] | 1.359 [1.199, 1.414] | 0.811 [0.750, 0.851] | 0.549 |
| 8 | 1.184 [1.088, 1.256] | 1.237 [1.203, 1.303] | 0.828 [0.766, 0.882] | 0.549 |
| 16 | 1.194 [1.157, 1.286] | 1.133 [0.980, 1.216] | 1.006 [0.914, 1.084] | 0.549 |

五档等待内存比值的区间上界均小于 0.57。4 worker 的跨线程 P99 比值为 0.356 [0.298, 0.404]。高 worker 的跨线程样本存在不同量级，结论以实测亲和范围为限。

TCP 与上一版有 240 个独立样本通过回显校验，服务端固定 CPU 0–7、客户端固定 CPU 8–15。32 连接 × 64 KiB 用例的往返 P99 在单 worker 与 4 worker 下降，其余指标格未检出稳定回退。

对两版 2 worker、百万任务 spawn/join 在 CPU 16–17 分别做了 2 ms 的 gprofng 采样。候选的 `SharedPool::finished` 及子调用占到 50.30% 的采样 CPU 时间，任务登记竞争仍出现在回收路径。此处是单次热点定位，收益以配对实验为准。

## 同一源码对 Tokio 1.54.1

五档 worker 的 100,000 任务各有 120 个正确性通过的样本，交替运行顺序与亲和范围相同。下表为 zrun/Tokio 的中位数比值。

| worker | spawn/join 吞吐 | 唤醒吞吐 | CPU / wake | 等待内存 / task |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 0.905 | 1.197 | 0.835 | 0.546 |
| 2 | 0.699 | 0.871 | 1.293 | 0.511 |
| 4 | 0.427 | 0.300 | 2.492 | 0.512 |
| 8 | 0.345 | 0.509 | 1.324 | 0.512 |
| 16 | 0.314 | 0.420 | 1.997 | 0.512 |

五档的等待内存和自唤醒 P99 均过普通门槛。单 worker 空闲内存也过线。其唤醒吞吐区间为 1.125–1.330，CPU / wake 区间为 0.761–0.888，两个区间仍未完全进入普通目标。4 worker 的跨线程 P99 比值为 0.469 [0.372, 0.537]。

将亲和范围扩大到 CPU 0–17 后，8 worker 的跨线程 P99 比值为 0.0939 [0.0007, 0.2258]，16 worker 为 0.000098 [0.000093, 0.008015]。两档在本机两种亲和范围均过线，巨大比值变化涉及微秒与数十毫秒两组样本，不能外推到其他负载。百万等待任务的 RSS / task 相对 Tokio 分别为单 worker 0.5455、8 worker 0.5148，两个置信区间均通过 0.60 的挑战门槛。

240 个 TCP 对照样本通过回显校验。8 worker、64 连接 × 64 B 时，吞吐比值达到 1.141 [1.018, 1.360]，往返 P99 为 0.470 [0.396, 0.627]。同为 8 worker 的 128 连接 × 4 KiB 和 32 连接 × 64 KiB 往返 P99 也过线，分别为 0.655 [0.437, 0.806] 与 0.722 [0.345, 0.919]。任务吞吐与多 worker CPU 仍有差距，完整跨平台矩阵仍待验证。

## 正确性与服务回归

70 项常规测试、11 项 Loom、30 项 Miri、严格 Clippy 与 rustdoc 通过。新增 Loom 模型覆盖最后执行引用、join 释放与取消升级的竞争。实际 Miri 路径覆盖内联和独立分配的 `!Unpin` Future、旧 waker、析构 panic 和跨线程取消。单 worker、8 worker 的百万任务均通过，8 worker 的 30 秒连续运行完成 22,430,000 项，活跃任务清零。

官方 SDK 的 MCP stdio、TCP 和 Unix 路径均通过，真实 PTY shell、zguard、zplugin 与 zmem scope 也通过。8 worker 的千并发 Unix 复用用例实际执行 1 次并合并 999 次。全 feature 的 zrun 核心与 CLI 常规依赖树中无 Tokio。

发布提交 `42d95e8` 与本地验收提交的源码树完全相同。该版另启了 4 worker 的 24 小时长测，固定 CPU 16–17，CPU 配额 100%，等待完成及 RSS 判定。

原始数据位于 Project Z 的 `dist`。

- `zrun-compact-final-pairs-w{1,2,4,8,16}/report.json`
- `zrun-compact-final-tokio-w{1,2,4,8,16}/report.json`
- `zrun-compact-final-tcp-{pairs,tokio}/report.json`
- `zrun-compact-final-memory-million/report.json` 与 `zrun-compact-final-tail-w{8,16}-wide/report.json`
- `zrun-compact-w2-spawn-{base,final}.er`、`zrun-compact-final-checks.log` 与 `zrun-compact-final-agent-load.json`
- `zrun-compact-final-soak-w8/report.json` 与 `zrun-soak-24h-managed-42d95e8-w4/report.json`
