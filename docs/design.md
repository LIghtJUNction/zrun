# zrun：设计与验证边界

zrun 的目标是建立独立于 Tokio 调度器的 Rust async runtime。第一阶段交付可以运行的单线程执行器，并把任务生命周期、唤醒、取消、定时器和退出语义固定下来。性能目标通过同机、同编译配置的独立进程重复实验判断；架构选择本身不作为性能收益证据。

## 模块职责

| 模块 | P0 职责 | 后续演进 |
| --- | --- | --- |
| zrun-task | 任务状态、Future 所有权、RawWaker、JoinHandle、协作 yield | 紧凑任务头、经模型验证的原子状态转换 |
| zrun-executor | 单线程本地就绪队列、block_on、spawn、容量、shutdown | P1 多 worker、本地队列和自适应 work stealing |
| zrun-time | 每 runtime 的定时器队列、唤醒、取消清理 | 经测量选择分层 wheel 或其他结构 |
| zrun-sync | 有明确关闭语义的一次性结果通道 | P1 有界异步 channel、同步原语 |
| zrun-bench | 同配置 zrun/Tokio 比较、原始样本 | 全规模矩阵、尾延迟、CPU 与内存分析 |
| zrun-io | P0 不实现 IO reactor | P2 Linux epoll；io_uring 为显式实验；P3 kqueue/IOCP |

P0 可以使用标准库提供的原子、互斥锁、线程 park/unpark 和堆结构；调度决策与生命周期由 zrun 自己实现。Tokio 仅出现在基准比较程序。P0 先保留可检查的状态机，再用证据判断哪些锁、分配和间接调用值得替换。

## 可调用的 P0 接口

`Runtime::new()` 创建单线程 runtime；`Runtime::with_max_tasks(n)` 设置明确的任务容量。`spawn` 接受可发送到执行器的 Future，返回 `Result<JoinHandle<T>, SpawnError>`；任务满额或 runtime 已关闭时不接收任务。`block_on` 在当前调用线程驱动根 Future、就绪任务和定时器。

`JoinHandle` 的 Future 输出是 `Result<T, JoinError>`。丢弃 join handle 会分离结果等待；`abort()` 显式请求取消。取消与完成竞争时，保留已经完成的结果；尚未完成的 Future 在拥有它的执行器线程处理终止。任务 panic 与取消应有可区分的错误。所有已接收任务最终只产生一个终止结果，Future 与结果各自只释放一次。

`Runtime::timer()` 返回可克隆的 Timer。`Timer::sleep(duration)` 产生 Sleep Future；驱动器负责到期唤醒，关闭时返回明确错误。没有每个定时器一个线程。正在等待的 timer 必须能注册和替换 waker，取消的定时器不得导致持续增长的保留内存。

P0 支持跨线程唤醒和通过 Spawner 提交 Send Future，但只有调用 block_on 的线程执行 poll。多 worker 调度属于 P1；不能把线程安全的唤醒入口计为 P1 已完成。

## 任务生命周期不变量

任务的调度标记必须区别“已在队列”“正在 poll”“poll 中再次收到 wake”和“已终止”。一个活跃任务同时至多有一个可执行队列项；poll 中的通知会在退出 poll 后保留为下一次调度请求。完成后到达的旧 waker 只释放其持有的引用，不重新 poll Future。

Future 的所有权必须在每次转换中明确。执行器取得 poll 权限后暂时拥有 Future；Pending 把它交回任务，Ready、abort、panic 与 shutdown 进入唯一终止路径。任务出队、取消或外部唤醒之间的竞争不能造成两份 Future 所有权，也不能释放正在 poll 的 Future。

RawWaker 中的指针代表一个真实的引用计数所有权。`clone` 增加一次所有权，`wake` 和 `drop` 各消耗传入的那一次，`wake_by_ref` 保留原所有权。注释必须解释每处 from_raw/into_raw 的配对、对象存活期和线程安全条件；正确性测试要覆盖最后一个 waker 释放、完成后的旧 waker、重复 wake 和取消风暴。

park 之前同时检查根 Future 的唤醒标记、就绪队列和最近定时器。检查与 park 之间到达的通知必须由 park/unpark 的令牌语义或等价协议保留，防止“队列已非空，但线程永久睡眠”。单批任务处理需要有限预算，避免大量自唤醒任务长期推迟根 Future 或定时器。

## 退出与反压

任务容量在接收 spawn 时执行；满额时返回错误，并释放未接收 Future。唤醒既有任务不因队列满而丢弃。默认任务容量以及 timer 容量由实现和 README 明确写出；使用百万任务实验时必须显式增加容量。

shutdown 停止接受任务，终止尚未完成的任务，唤醒等待结果的 join handle，清理 timer 和队列。Runtime drop 走同一条生命周期。跨线程持有的旧 Spawner 和 Waker 不得让已关闭 runtime 复活。P0 不抢占一个始终不返回的 Future::poll；这是协作式 async 的执行边界。

## 性能验收协议

普通目标与挑战目标分别记录，不通过改工作负载或只展示最佳一次来满足门槛。

| 指标 | 普通目标（相对 Tokio） | 挑战目标 |
| --- | --- | --- |
| spawn + join 吞吐 | ≥ 1.2× | ≥ 2× |
| wake 吞吐 | ≥ 1.2× | ≥ 2× |
| 调度开销 | ≤ 85% | ≤ 50% |
| 每任务内存 | ≤ 85% | ≤ 60% |
| 空闲增量 RSS | ≤ 80% | ≤ 50% |
| P99 wake 延迟 | ≤ 100% | ≤ 70% |
| TCP 吞吐 | ≥ 95% | ≥ 120% |
| TCP P99 延迟 | ≤ 110% | ≤ 80% |

完整矩阵为 1、2、4、8、16 worker 与 1k、10k、100k、1m task，每个配置至少 10 个独立进程样本。每组记录中位数、P95/P99、置信区间、原始样本、CPU、内核、Rust 版本、源码提交和编译参数。P0 先比较单线程；多 worker 与 TCP 行在其对应阶段实现前保持未执行。

内存同时观察运行中任务的 RSS 增量、每任务摊销与任务结束后的稳态。分配器保留高水位容量要与持续增长区分；一次低 RSS 读数不替代长期实验。百万任务必须实际验证无丢任务和死锁；24 小时 soak 必须记录过程样本、退出状态及斜率，完成之前标为未完成。

## 平台与工具

P0 的标准库调度器面向 Linux、macOS、Windows，实际平台结果分别记录。Linux epoll 首发不把 IO 模块提前包装成跨平台完成。io_uring 独立于稳定 epoll 路径，明确内核要求和实验开关。

针对原子状态机准备 Loom 模型，对 RawWaker 与 Future 所有权运行 Miri。模型覆盖与真实实现的映射应写入测试说明；替身状态模型通过不能替代真实运行测试。取消风暴、跨线程唤醒、panic、容量边界、timer 关闭及运行时关闭均需要可重复回归。

本次只允许源码、设计、测试和基准，不部署或发布。
