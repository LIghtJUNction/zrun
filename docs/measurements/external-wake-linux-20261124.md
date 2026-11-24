# Linux 跨线程唤醒与同线程通知实验

比较器和采样脚本以 `aee307c` 为基准，固定 Tokio 1.54.1 和 Rust 1.100.0 release 构建。在 Intel Core i9-12900HX、Linux 7.3.5-arch1-1 上执行，所有进程包括唤醒生产线程使用 CPU 0–15。各配置 10 对独立进程，逐轮交换顺序。所有区间为配对 bootstrap 95% 区间。

## 专用线程发出唤醒

每个任务首次 poll 时提交自身的 Waker。一个额外线程等全部注册后唤醒每个任务一次，按固定索引采样，起点在发布就绪信号之前，终点在任务恢复 poll 时。延迟包括通过执行器队列等候和 worker 交接。每个样本确认任务标识、完成数、校验和以及退出时无活动任务。120 个样本全部通过正确性检查，6 个配置均未达到严格领先门槛。

| worker | 任务数 | zrun P99 | Tokio P99 | zrun/Tokio，95% 区间 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 10,000 | 2.529 ms | 1.270 ms | 1.992 [1.662, 2.355] |
| 1 | 100,000 | 26.090 ms | 11.078 ms | 2.355 [1.933, 2.481] |
| 4 | 10,000 | 3.790 ms | 2.270 ms | 1.670 [1.446, 1.993] |
| 4 | 100,000 | 49.657 ms | 25.900 ms | 1.917 [1.635, 2.295] |
| 8 | 10,000 | 3.225 ms | 2.202 ms | 1.464 [1.193, 1.682] |
| 8 | 100,000 | 49.266 ms | 27.715 ms | 1.778 [1.507, 2.056] |

```sh
cargo build --locked --release -p zrun-bench --bin zrun-bench
python3 scripts/benchmark.py --binary target/release/zrun-bench \
  --output /var/tmp/zrun-external-wake --scenarios external_wake_latency \
  --workers 1 4 8 --tasks 10000 100000 --rounds 10 --affinity 0-15
```

## 任务自身唤醒的候选

`a95e60c` 在任务自己的 poll 中用线程局部标记记录通知，其他唤醒仍使用共享任务状态。普通回归包括嵌套 poll 恢复外层标记，以及另一线程在当前 poll 内唤醒。与 `35729ae` 配对比较 spawn/join、自唤醒吞吐、自唤醒 CPU 和 P99，360 个进程样本全部通过正确性检查。五个自唤醒 P99 配置下降，但 8 worker、10,000 tasks 的自唤醒吞吐降到 0.911× [0.841, 0.986]，CPU / wake 增至 1.153× [1.007, 1.216]。

另外在外部唤醒比较器中应用同一候选（`d6c718c`），与 `aee307c` 比较，120 个样本正确。跨线程 P99 的六个区间均包含 1。候选已撤下。保留新的跨线程基准，以便同版本同时核对自唤醒和跨线程尾延迟。

原始数据在 Project Z 的 `dist/zrun-external-wake-tokio/report.json`、`dist/zrun-external-wake-w{1,4,8}/report.json` 与 `dist/zrun-local-wake-w{1,4,8}/report.json`。候选补丁在 `dist/zrun-local-wake-candidate.patch`。
