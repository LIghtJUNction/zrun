# zrun

zrun 提供独立的 Rust 异步运行时和面向 AI agent 的 Linux 命令执行服务。终端、批量脚本和 MCP 客户端可以向同一个进程提交命令。服务为可复用命令合并并发请求，缓存有明确输入版本的成功结果，并限制进程数、排队字节数和输出大小。

运行时的长期目标是成为 Tokio 的替代选项。当前核心提供 **P0 单线程调度器、任务生命周期、定时器和结果通道**，另外通过 `io` feature 提供异步 TCP、Unix socket 和 Unix 文件描述符。单线程可以用 `Runtime::block_on_with_io` 同时推进任务、定时器和 I/O。Tokio 位于独立比较程序，多线程候选 `Pool` 与性能门槛见[阶段计划](docs/plan.md)。

**0.1.0-p0.15** 是 P0 预览版。Linux 四 worker、10 万任务下，本版普通 spawn/join 吞吐为原主线的 2.31 倍、自唤醒吞吐为 5.21 倍，分别达到同机 Tokio 的 0.28 倍和 0.59 倍。16 worker 竞争提交的首次执行 P99 出现回退。完整条件见[本版 Tokio 对照](docs/measurements/p0.15-tokio-matrix.md)和[发行说明](docs/releases/0.1.0-p0.15.md)。


## 执行命令

在 Linux 终端直接运行 `zrun` 会进入 zrun 自己的 shell。它保存当前目录和变量，执行普通命令，也能用 `async 名称 -- 命令` 启动作业、用 `await 名称` 取回输出。`zrun run` 执行一条受管命令，`zrun batch` 从 stdin 读取 JSONL 并发执行。多个进程可以用 `ZRUN_SOCKET` 接入同一个本地队列。远程批处理可用 `batch --ready` 优先接收已完成的批次。

```sh
cargo build --locked --release -p zrun-cli
target/release/zrun
target/release/zrun run 'printf "%s\n" ready'
```

运行 `zrun FILE` 可执行 zrun 语法的普通脚本。文件首行的 shebang 可显式选择其他解释器。Linux 上在首行加入 `# zrun:1` 即可用 `async` 提交命名任务，再用 `await` 等待指定任务。设置 `ZRUN_SOCKET` 可让多个脚本共享执行队列和显式复用缓存。

[脚本语法](docs/native-script.md) · [MCP、Unix socket、复用协议和 Project Z 集成](docs/agent-shell.md) · [GitHub 预览发行](https://github.com/LIghtJUNction/zrun/releases/tag/v0.1.0-p0.15)

Omarchy 状态栏可实时查看 MCP 命令执行和复用计数。见 [Zrun for Omarchy](packaging/omarchy/README.md)。

## 运行一个任务

需要 Rust 1.100.0。下面的例子创建任务，让出一次执行机会，等待定时器，再取回结果：

```rust
use std::time::Duration;
use zrun::{Runtime, yield_now};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::with_max_tasks(1024)?;
    let timer = runtime.timer();

    let task = runtime.spawn(async move {
        yield_now().await;
        timer.sleep(Duration::from_millis(10)).await?;
        Ok::<u32, zrun::TimerError>(42)
    })?;

    let result = runtime.block_on(task)??;
    println!("result = {result}");
    runtime.shutdown();
    Ok(())
}
```

完整文件是 [examples/p0.rs](examples/p0.rs)：

```sh
cargo run --locked --example p0
cargo test --locked --workspace
cargo test --locked --test runtime one_million_tasks_have_no_loss -- --ignored --exact
```

Project Z 开发机的 Cargo 命令必须进入仓库约定的单个外层共享构建锁；上面只展示标准 Cargo 参数。不要在已有锁内再次获取同一把锁。

多线程场景可使用 `zrun::Pool::new(4)`，通过 `spawn` 或 `spawner` 提交任务，再用 `block_on` 等待结果。每个 worker 维护自己的就绪队列，闲置时可接手同池任务。现有 `Runtime` 继续使用单线程调用方式。启用 `io` 后，`Pool::with_max_tasks_and_io` 返回 Pool 及其首个 worker 驱动的 I/O handle，可用来注册 TCP 和 Unix socket。

## 从 Future 到结果

Rust 的 async 函数生成一个保存执行进度的 Future。Future 返回 Pending 时，执行器不能忙等；它保存任务，等待某个 Waker 说明任务可能继续执行。zrun 用任务状态和就绪队列接住这个通知，再由调用 block_on 的线程继续 poll。

P0 只有一个线程执行任务的 poll。其他线程可以通过 Spawner 提交 Send Future，也可以唤醒已有任务。跨线程入口并不意味着多个任务会并行执行。block_on 的根 Future 可以借用本地数据，也可以包含不能跨线程发送的值。

一个任务同时最多拥有一个有效的就绪队列项。poll 期间再次发生 wake 时，通知保留到此次 poll 结束；任务结束后收到旧 wake 时，不重新执行已经释放的 Future。具体所有权与状态不变量见 [设计](docs/design.md)。

## API 与容量

| API | 行为 |
| --- | --- |
| Runtime::new() | 默认最多 65,536 个活跃任务；按需分配 |
| Runtime::with_max_tasks(n) | 显式设置任务和定时器容量；0 非法 |
| Runtime::spawn / Spawner::spawn | 接收 Send Future；满额或关闭时返回错误 |
| Runtime::block_on | 驱动根 Future、就绪任务和到期定时器 |
| JoinHandle::await | 返回任务结果，或取消、panic、关闭错误 |
| JoinHandle::abort | 请求取消；已完成的结果不被覆盖 |
| 丢弃 JoinHandle | 分离结果等待，不自动取消任务 |
| Runtime::timer().sleep(duration) | 由同一 runtime 的定时器队列驱动 |
| Runtime::shutdown | 停止接收任务，结束未完成任务并清理等待状态 |
| Runtime::stats | 观察活跃任务、就绪任务与定时器条目 |

定时器容量与任务容量相同，默认 65,536。定时器不创建专用线程；取消直接移除登记，避免长期积累已取消条目。定时器必须由所属 runtime 的 block_on 驱动。

zrun 是协作式调度器。一个从不返回的 Future::poll 会阻塞该执行线程；取消不是对任意 Rust 代码的抢占。CPU 密集工作需要由应用拆成有限片段，或交给应用自行管理的线程。

## 模块

| Crate | 当前职责 |
| --- | --- |
| zrun | 应用使用的公共入口 |
| zrun-task | RawWaker、任务状态、JoinHandle、yield |
| zrun-executor | 有界任务接收、单线程队列、block_on、关闭 |
| zrun-time | 定时器登记、到期唤醒和取消清理 |
| zrun-sync | 任务结果所使用的同步原语 |
| zrun-bench | 与 Tokio 的独立进程比较及持续运行工具 |
| zrun-io | 可选的 TCP、Unix socket 和描述符就绪通知 |
| zrun-cli | 交互 shell 入口、统一命令队列、MCP 和复用 |
| zrun-plugin-api | zplugin 原生命令准入 ABI |

默认调度核心使用标准库与本仓库 path 依赖。可选 I/O 模块使用 Mio，应用中的授权、插件 ABI 和二进制输出复用 zguard、zplugin 和 z64。

## 验证与测量

2026-11-23 的 [P0 验收与首次基准](docs/measurements/p0-linux-20261123.md) 已记录实际结果：43 项常规测试、显式百万任务测试、35 项核心 Miri 检查和 400 个比较进程样本通过正确性检查；120 秒短测结束时没有活动任务。首次性能实验尚未达到普通或挑战门槛。后续休眠通知优化通过了 3 个 Loom 模型，见下方配对实验；24 小时稳定性、多线程与 I/O 仍未完成。

集成测试覆盖任务容量、拒绝时释放、重复取消、panic 隔离、跨线程唤醒风暴、完成后的旧 Waker、关闭后的 Spawner、借用根 Future、定时器到期和取消清理。百万任务测试默认忽略，必须用上面的显式命令执行才可记录通过。

比较程序使用同一编译配置、相同任务和独立进程采样。默认协议要求每个配置至少 10 次，并保留原始样本、硬件和编译信息。具体定义与运行参数见 [benchmark 协议](docs/benchmark-protocol.md)。

全面超越 Tokio 需要同一候选版本同时满足任务吞吐、调度开销、内存、唤醒尾延迟和 TCP 门槛。spawn/join 与 wake 吞吐至少为 Tokio 的 1.2 倍，调度开销与每任务内存不高于 85%，空闲增量 RSS 不高于 80%。自唤醒、跨线程唤醒和 TCP 的 P99 均须低于 Tokio，TCP 吞吐须高于 Tokio。完整目标、挑战目标和各指标方向见 [设计中的门槛表](docs/design.md#性能验收协议)。没有测量的数据保持未验证。

P0 的历史结果只对应单 worker。当前比较程序支持 1/2/4/8/16 worker，TCP 与跨平台测量沿用各阶段门槛。Loom、Miri、百万任务、取消风暴与 24 小时 soak 的结果需要分别记录，短期测试不能替代长期稳定性验证。

## 后续阶段

2026-11-24 的 [任务登记实验](docs/measurements/task-registry-linux-20261124.md)记录了 45 项常规回归、显式百万任务回归、37 项核心 Miri、3 项休眠协议 Loom，以及共 800 个正确性通过的比较样本。相对本轮基线的百万任务吞吐和 self-wake 延迟有所改善；新的 Tokio 比较仅有 2/20 个规模×指标单元通过目标，完整门禁仍未完成。

[阶段计划](docs/plan.md)列出 P0 到 P3 的交付门槛。优化前先保留可复现的基线，优化后保持同一工作负载与统计方法。只有真实完成的功能和测量进入完成列表。

[上层依赖替换计划](docs/adoption.md)明确 zrun 与 zharness、ssmcp、zaccel 的职责边界、可迁移代码和接入门槛。底层执行能力只维护一份，上层保留自己的业务状态和资源归属规则。

MIT License。

## P0 通知路径后续实验

[2026-11-23 配对实验](docs/measurements/p0-parking-20261123.md)记录了准备休眠时才发送线程通知的修改、400 个独立进程样本，以及通过的 Loom/Miri 检查。三个规模的自唤醒吞吐有改善证据，spawn 与每任务内存没有明确收益；相对 Tokio 的整体验收仍未通过。

### 发布运行时库

从 GitHub Actions 的 **Release runtime crates** 选择 `main`，填写要发布的完整提交 SHA。该提交须属于所选 `main` 的历史。默认运行三平台 CI，在 Linux 上打包并编译六个运行时 crate，再逐个核对 crates.io 已有版本的校验值。勾选 `publish` 后，工作流按依赖顺序上传缺少的包。重复运行会跳过内容一致的版本，发现已撤回的版本或内容冲突时会停止。

上传使用本仓库的 `CARGO_REGISTRY_TOKEN`，或为六个 crate 配置指向 `LIghtJUNction/zrun` 的 `release-runtime.yml` 可信发布者。GitHub 预览版通过 **Release zrun P0** 单独发布，可填写同一提交 SHA 来生成二进制与源码归档。
