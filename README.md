# zrun

zrun 是一个独立的 Rust async runtime。当前阶段是 **P0：单线程调度器、任务生命周期、定时器和结果通道**。它自己决定何时 poll Future、如何保存 wake、如何取消和关闭任务；运行时不调用 Tokio 的调度器。Tokio 只用于独立比较程序。

当前发行是 **0.1.0-p0.1 预览版**，提供可复现源码与验证流程。性能目标仍是后续验收条件；首次测量尚未达到这些目标。参见 [发行说明](docs/releases/0.1.0-p0.1.md)。

本工作副本正在验证 **0.1.0-p0.2 候选版**，尚未发布或部署。它保留休眠通知优化，并将活动任务登记改为连续引用数组；索引、对象身份和关闭边界见 [任务登记设计](docs/task-registry.md)。

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

核心模块只使用标准库与本仓库 path 依赖。比较程序的 Tokio 依赖不会把 Tokio 调度器引入 zrun 的执行路径。

## 验证与测量

2026-11-23 的 [P0 验收与首次基准](docs/measurements/p0-linux-20261123.md) 已记录实际结果：43 项常规测试、显式百万任务测试、35 项核心 Miri 检查和 400 个比较进程样本通过正确性检查；120 秒短测结束时没有活动任务。首次性能实验尚未达到普通或挑战门槛。后续休眠通知优化通过了 3 个 Loom 模型，见下方配对实验；24 小时稳定性、多线程与 I/O 仍未完成。

集成测试覆盖任务容量、拒绝时释放、重复取消、panic 隔离、跨线程唤醒风暴、完成后的旧 Waker、关闭后的 Spawner、借用根 Future、定时器到期和取消清理。百万任务测试默认忽略，必须用上面的显式命令执行才可记录通过。

比较程序使用同一编译配置、相同任务和独立进程采样。默认协议要求每个配置至少 10 次，并保留原始样本、硬件和编译信息。具体定义与运行参数见 [benchmark 协议](docs/benchmark-protocol.md)。

普通目标包括 spawn/join 与 wake 吞吐至少为 Tokio 的 1.2 倍、调度开销与每任务内存不高于 85%、空闲增量 RSS 不高于 80%、P99 wake 延迟不高于 100%。完整目标、挑战目标和各指标方向见 [设计中的门槛表](docs/design.md#性能验收协议)。没有测量的数据保持未验证。

P0 只比较单 worker。多 worker 队列与窃取属于 P1；Linux epoll、TCP 测量属于 P2；kqueue/IOCP 属于 P3。Loom、Miri、百万任务、取消风暴与 24 小时 soak 的结果需要分别记录，短期测试不能替代长期稳定性验证。

## 后续阶段

[阶段计划](docs/plan.md)列出 P0 到 P3 的交付门槛。优化前先保留可复现的基线，优化后保持同一工作负载与统计方法。只有真实完成的功能和测量进入完成列表。

[上层依赖替换计划](docs/adoption.md)明确 zrun 与 zharness、ssmcp、zaccel 的职责边界、可迁移代码和接入门槛。底层执行能力只维护一份，上层保留自己的业务状态和资源归属规则。

MIT License。

## P0 通知路径后续实验

[2026-11-23 配对实验](docs/measurements/p0-parking-20261123.md)记录了准备休眠时才发送线程通知的修改、400 个独立进程样本，以及通过的 Loom/Miri 检查。三个规模的自唤醒吞吐有改善证据，spawn 与每任务内存没有明确收益；相对 Tokio 的整体验收仍未通过。
