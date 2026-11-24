# Linux MCP 原始结果预算

比较 `ebd850b` 与 `efdd29123aa25998d39da096bf20608baa0fdd65`。两版均使用 Rust 1.100.0 release 构建。测试运行在 Intel Core i9-12900HX、Linux 7.3.5-arch1-1，进程固定在逻辑 CPU 0–7。每个样本启动新服务，两版顺序逐轮交替。

## 大批次和慢客户端

4 worker，96 个客户端各提交一批 16 条独立命令。每条命令产生 64 KiB stdout 和 64 KiB stderr，并记录唯一完成标记。客户端暂停读取响应体，待完成标记停止增长且没有子进程时采样服务 RSS，再继续读取。每对样本检查所有 1,536 条命令只执行一次，每条响应的 ID、退出码、状态和输出均完整，两版交付的 JSON 总量同为 403,210,454 字节。

| 5 对样本 | 旧版 | 新版 |
| --- | ---: | ---: |
| 开始读取响应体前完成的命令 | 1,536 | 1,136 |
| 服务 RSS 中位数 | 217,904 KiB | 166,660 KiB |
| 整批用时中位数 | 2.390 s | 2.634 s |

RSS 新旧比的 95% 区间为 0.763–0.766，整批用时比为 1.026–1.148，均以 5,000 次配对 bootstrap 计算。新版先完成 7 个已进入编码缓冲的批次和 64 个占用原始结果预算的批次。其余批次等待原始结果预留空间，在读取开始后继续执行。整批用时包括背压测试中等待命令数稳定的停顿、子进程、结果发送和客户端解码。

TCP 与 Unix 使用同一个 broker。在开始提交命令前一次预留整批的原始结果空间，等待提交的 HTTP 请求继续占用接收预算。命令返回后，按原始 stdout/stderr 的实际容量缩减预留。完成编码并释放原始结果引用后释放余下预留。已经缓存的结果还有独立的缓存上限；RSS 同时包括进程其他内存，内核 socket 缓冲另计。

```sh
taskset -c 0-7 python3 scripts/agent-load.py --binary /path/to/new/zrun \
  --raw-output-baseline /path/to/old/zrun --agents 96 --commands-per-request 16 \
  --workers 4 --rounds 5
```

## 普通千并发

独立执行和复用各跑 5 对。两组的整批用时比 95% 区间均包含 1。复用组仍为 1 次执行、999 次合并，峰值 RSS 中位数由 6,684 增至 6,760 KiB。工作区 64 项测试通过、1 项跳过，严格 Clippy、rustdoc 和 release 构建通过。

原始样本在 Project Z 的 `dist/zrun-raw-budget-{1536-w4,ordinary-1000}.jsonl`。两版二进制 SHA-256 分别为 `3acb34b64888925d48d86430878beab5ae60b28df8876bae96c03924cdc3bc97` 和 `25c508e30b8d65504582e0a6d2ad78454600351998eb6d88d098e94de99abe1c`。
