# 分阶段实施

## P0：本轮交付

1. 提交模块边界、生命周期不变量、公开接口和性能协议。
2. 实现单线程 spawn、block_on、JoinHandle、yield 和 timer。
3. 加入重复 wake、跨线程 wake、取消/丢弃、panic、容量、timer 与 shutdown 回归。
4. 建立 zrun/Tokio 同配置单线程比较程序；每次独立进程采样，保留原始结果与统计脚本。
5. 按实际运行结果填写验收记录并形成源码提交。性能未达标的指标保留原值。

## P1：多线程调度

候选实现提供 `Pool` 与跨线程 `PoolSpawner`。工作线程维护各自队列，空闲者从同池队列按有限批次窃取任务。定时器由首个 worker 推进，根 Future 仍在调用线程执行。`Runtime` 保留 P0 单线程入口。候选只有在完成下方性能和生命周期门槛后才进入正式发行。

继续验证队列迁移、运行与取消竞争、shutdown 排空及跨 worker timer 唤醒。执行 1/2/4/8/16 worker 全矩阵、Loom、Miri、百万任务和取消风暴；并发正确性先于性能结论。Linux 的 100,000 任务已覆盖五档 worker。合并任务分配后，五档的等待内存和自唤醒 P99 已过 Tokio 普通门槛。完整矩阵仍待验收，当前版的四 worker 24 小时长测已启动。[实测与回归](measurements/compact-task-linux-20261124.md)

调度批次与定时器短路径已做同机筛选。单 worker 的唤醒吞吐、CPU 和内存已有同时过线的候选，2 worker 创建与回收及 16 worker CPU 回归可复现。[调度对照、复测和命令服务回归](measurements/caller-timer-experiments-linux-20261124.md)

多线程任务登记分片已合入。2、4 worker 的创建与回收及唤醒负载的 CPU 开销均有改善。4 worker 的新一轮 24 小时长测已启动。[分片实测](measurements/sharded-registry-linux-20261124.md)

## P2：TCP IO

可选 `io` 模块已有由 Mio 驱动的 TCP、Unix socket 与 Unix 文件描述符就绪通知。它在 Linux 使用 epoll。注册令牌在单个 reactor 内不重复，停止驱动会唤醒等待者。单 worker 用同一线程推进任务和 I/O。分片版 Linux 回环 TCP 的 24 个指标格有 2 个过线，覆盖 8 worker 的两档往返 P99。更完整的连接与背压测试继续验收。io_uring 单独作为实验。[当前同版本 Tokio 对照](measurements/sharded-registry-linux-20261124.md)

## P3：平台与优化

实现并分别验证 macOS kqueue 与 Windows IOCP。根据 CPU profile 和内存数据优化任务布局、调度批量、偷取阈值和 timer 结构；每个优化都必须保留正确性门禁。完整 24 小时 soak 和跨平台 CI 结果进入正式发布决策。2026-11-23 用户已授权公开 P0 预览发行；完整正式版仍按上述性能、稳定性和平台门禁决定。

## 命令服务

[命令协议](agent-shell.md) 规定 CLI、MCP 和 Unix socket 的共用入口，并定义复用键、输出、排队和接入语义。Linux 命令服务已完成首轮本地验证。Linux 的 TCP 和 Unix socket 已验证共享接收缓冲预算和过载恢复。不同命令的大结果已验证有界的原始结果与编码预算，以及背压下的完整交付。更多 worker 配置、持续过载和其他平台的托管进程实现继续单独验收。

## 状态记录规则

设计完成、源码可编译、测试通过、实测性能达标和长时间稳定性通过是独立状态。每份报告写明实际命令、提交和原始数据位置。尚未运行的工具和平台保持未验证；不以工作流文件已经存在代替运行结果。
