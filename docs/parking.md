# P0 的休眠通知握手

执行器运行时，根 Future 的原子通知标记、受互斥锁保护的就绪队列和 timer 队列已经保存了待处理工作。原实现仍在每次通知时锁住线程句柄并调用 `unpark`。本次只修改这条通知路径；任务分配、队列顺序、64 次任务预算、取消、JoinHandle 和 timer 数据结构保持原实现。

## 如何避免丢失唤醒

[ParkingFlag](../crates/zrun-executor/src/parking.rs) 在执行器检查三个工作来源之前设置等待标记。生产者先发布工作，再清除该标记。如果它取得原来的 `true`，它负责向运行线程发送一个 `unpark` 令牌；其他通知可以合并。即使运行线程尚未调用 `park`，标准库的线程令牌也会保留这次通知。

所有标记变化使用 acquire/release 原子 read-modify-write。通知发生在设置等待标记之前时，设置动作取得前面的发布结果，后续检查能看见工作；通知发生在设置标记之后时，取得等待标记的通知者必须发送线程令牌。取消一次等待、超时、虚假唤醒或根 Future 展开都清除标记。清除动作也保留 acquire/release 链，避免把一个发生在活跃阶段的发布隔断。

`false` 表示无需再分配一次线程通知的责任；它并不证明队列为空，也不能代替三个工作来源的检查。执行器只有在设置标记、完成检查且没有工作可运行时才进入 `park`。通知者在取得责任后暂停执行仍须最终调用 `unpark`，这是底层线程正常运行的活性前提。

## 模型与真实实现的对应

三个 Loom 测试编译同一个 `ParkingFlag` 实现，只替换原子类型。模型覆盖发布发生在等待设置前后、两个通知者竞争以及取消等待后再次设置等待。模型中的 `ready` 使用比实际根通知更弱的 relaxed 发布，以检查握手本身的同步作用。模型中的 `token` 表示标准库承诺保留的线程通知；模型不重新实现操作系统调度，也不证明任务队列、RawWaker 或完整执行器的全部状态空间。

```sh
RUSTFLAGS="--cfg loom" cargo test --locked -p zrun-executor --lib model_
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo miri test --locked -p zrun-executor --lib
```

普通执行器回归另外覆盖跨线程注入、重复 wake、取消、根 panic 后再次驱动、timer 进展和 shutdown。实际执行记录与性能样本存放在交付证据中；加入测试命令本身不代表它已通过。

## 修订之间的比较

[compare_revisions.py](../scripts/compare_revisions.py) 使用原有比较程序的相同 Future、16 次自唤醒、任务规模、延迟采样和 RSS 口径。每个修订、场景、规模和轮次均启动独立进程。每对相邻进程在偶数轮先运行基线，奇数轮先运行候选。两个版本始终标记为 `engine=zrun`。

运行前必须提供两个干净源码构建的 JSON 回执，包括 `exit_code: 0`、完整 `source_head`、空的 `source_status` 和实际 `binary_sha256`。脚本校验二进制摘要并保存回执。结果的比值是候选中位数除以基线中位数，置信区间按配对轮次进行 5,000 次 bootstrap。至少十轮完整 release 样本且整个 95% 区间越过 1，才记录该场景的改进证据。

```sh
python3 scripts/compare_revisions.py \
  --baseline /path/to/baseline-bench --baseline-build /path/to/baseline-build.json \
  --candidate /path/to/candidate-bench --candidate-build /path/to/candidate-build.json \
  --output /path/to/new-results --rounds 10 --affinity 4
```

这项比较只隔离本次通知路径修改的效果。它不将旧版本 zrun 冒充 Tokio，也不更新对 Tokio 的验收状态。共享机器负载、RSS 页粒度和未控制的调频仍限制结论；多 worker、TCP、调度 CPU 占用和 24 小时 soak 仍需要各自的实际证据。
