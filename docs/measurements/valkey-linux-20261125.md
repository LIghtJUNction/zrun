# Valkey 跨 broker 结果缓存

源码 `f20d947`，Linux x86_64、Intel Core i9-12900HX、Rust 1.100.0 release、Valkey 9.2.0，经 Unix socket 连接。broker 为四 worker，测试进程固定 CPU 0–15。每轮使用 500 个并发 MCP 客户端、500 个不同复用键。命令执行 `sleep .015`，并写入计数文件，测试以该文件和返回状态交叉核对实际启动次数。

每组三种模式使用新的 broker 进程，顺序轮换。热缓存由另一个已退出的 broker 预先写入。未将 broker 启动和预热计入测量。10 组中，试验样本共验证 15,000 个回包。冷缓存和关闭模式每轮实际启动 500 次，热缓存每轮 500 个结果均为缓存命中、实际启动零次。

| 模式 | 完成耗时中位数 | broker 峰值采样 RSS 中位数 | 峰值直接子进程数中位数 |
| --- | ---: | ---: | ---: |
| 关闭 Valkey | 315.5 ms | 5,962 KiB | 32 |
| Valkey 冷缓存 | 320.0 ms | 6,046 KiB | 32 |
| 重启 broker 后命中 Valkey | 39.9 ms | 5,010 KiB | 0 |

相比关闭 Valkey，冷缓存耗时比 1.014 [1.002, 1.030]，热缓存耗时比 0.126 [0.118, 0.139]。热缓存的 broker RSS 比 0.840 [0.833, 0.849]。区间为 5,000 次配对 bootstrap。RSS 只计 broker，未计入 Valkey 或子进程。这项测试测量已完成结果跨 broker 复用，不覆盖分布式在途合并，也不适用于其他命令成本或缓存命中率。

复现：

```sh
taskset -c 0-15 python3 scripts/valkey-load.py --binary /path/to/zrun --agents 500 --rounds 10
```

原始样本位于 `dist/zrun-valkey-load-results.jsonl`。
