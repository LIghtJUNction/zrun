# Linux MCP 输出内存

比较 `59a29c8` 与 `faa3c1a08be86d9e966fae1c5389c9cc45a5a5ed`。两版都使用 Rust 1.100.0 release 构建，运行于同一台 Intel Core i9-12900HX（24 逻辑 CPU）、Linux 7.3.5-arch1-1。每个样本启动独立服务，轮次间交换版本顺序。

## 缓存结果与慢客户端

先执行一次命令，将 stdout 和 stderr 各 64 KiB 的双引号数据写入缓存。192 个 Unix socket 客户端马上请求同一结果。每个客户端读到响应头后暂停主动读取，全部就绪后等待 100 ms，读取服务进程的 `/proc/<pid>/status` RSS，再排空所有连接。每个样本校验所有客户端的 RPC ID、缓存命中、输出和文本与结构化结果的一致性。两个版本每次都交付 151,083,156 字节 JSON，采样时均无子进程。

| worker | 配对轮数 | 等待写出的 RSS 中位数，旧版 → 新版 | 新版/旧版 RSS 的 95% 区间 | 全部响应头就绪中位用时 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 10 | 153,730 → 6,028 KiB | 3.92%–3.92% | 226 → 31 ms |
| 4 | 5 | 156,860 → 6,440 KiB | 4.10%–4.11% | 66 → 30 ms |

1 worker 的旧版 RSS 样本范围为 153,696–153,880 KiB，新版为 6,028–6,032 KiB。4 worker 的范围分别为 156,676–156,988 KiB 和 6,432–6,440 KiB。区间用 5,000 次配对 bootstrap 计算，百分比保留两位小数后，第一行两端相同。

旧版为每条连接拼成一份完整响应，慢客户端让这些缓冲区同时驻留。新版在交付期间共享命令输出的编码，使用向量写入发送同一个输出和每条请求各自的 ID、状态。最后一个响应释放后，缓存条目只保留原始输出。RSS 只计服务进程，不包含客户端或内核 socket 缓冲区。大量彼此不同的输出与全局字节预算仍需单独验证。

```sh
python3 scripts/agent-load.py --binary /path/to/new/zrun \
  --slow-output-baseline /path/to/old/zrun --agents 192 --rounds 10 --workers 1
```

## 普通千并发

照 [命令服务负载](agent-shell-linux-20261124.md) 执行 `sleep 0.05; printf zrun`，1 worker、5 对样本。两个版本的独立执行组均执行 1,000 次，复用组均为 1 次执行、999 次合并，所有结果通过校验。

| 模式 | 峰值 RSS 中位数，旧版 → 新版 | 新版/旧版 RSS 的 95% 区间 | 整批中位用时，旧版 → 新版 |
| --- | ---: | ---: | ---: |
| 独立执行 | 9,256 → 6,872 KiB | 73.9%–75.7% | 1.714 → 1.725 s |
| 复用 | 9,820 → 6,692 KiB | 67.5%–68.4% | 102.2 → 103.5 ms |

整批用时比的区间均包含 1，未测得稳定变化。本轮还复验了官方 MCP Python SDK 1.27.2 的 Unix、TCP 和 stdio 转发，以及 zmcpio 0.5.3 对 Unix socket 的原生接入。工作区测试 60 项通过、1 项按配置跳过，严格 Clippy、rustdoc 和 release 构建通过。

原始样本位于 Project Z 的 `dist/zrun-shared-reply-slow-192.jsonl`、`dist/zrun-shared-reply-slow-192-w4.jsonl` 和 `dist/zrun-shared-reply-ordinary-1000.jsonl`。两版二进制 SHA-256 分别为 `d957019773e9b1539d3e84246f96491d2fb0a8af44aa335388c72a62a7c56070` 和 `bc0e75e489871eec8e01af731a1bc0cce71b1f4158587d8cb009e37178db357e`。
