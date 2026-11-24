# Linux MCP 请求缓冲

比较旧版 `3ad5ae1` 和新版 `d2c64f80dd6e76cc16110d96fe5ec5ccca6ee451`。两版均为 Rust 1.100.0 release 构建。测试运行在 Linux 7.3.5-arch1-1、Intel Core i9-12900HX，上下文固定在逻辑 CPU 0–7。每个样本启动新服务，版本顺序逐轮交换，原始样本保留。

## 仅收到请求头

384 个 Unix 客户端都声明 256 KiB 的 HTTP 请求体，发完请求头后暂停。确认服务已经接收所有连接，间隔 100 ms 采样服务进程 RSS。每个样本随后让其中 8 条连接发送完整的 256 KiB 请求体，核对 JSON-RPC ID 和响应内容。采样时没有子进程。

| worker | 配对轮数 | 旧版 RSS 中位数（范围） | 新版 RSS 中位数（范围） | 新版/旧版 RSS 的 95% 区间 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 10 | 104,832（104,808–104,868）KiB | 5,210（5,096–5,232）KiB | 4.94%–4.98% |
| 4 | 5 | 104,916（104,908–104,948）KiB | 5,296（5,180–5,308）KiB | 4.94%–5.06% |

区间用 5,000 次配对 bootstrap 计算。旧版按 `Content-Length` 直接分配并填充整个接收缓冲。新版随实际读取增长缓冲，分块请求在同一缓冲内解码。另用新版跑了两次 1,000 个连接，采样 RSS 为 6,868 和 6,876 KiB。此处 RSS 只计服务进程。

```sh
taskset -c 0-7 python3 scripts/agent-load.py --binary /path/to/new/zrun \
  --slow-input-baseline /path/to/old/zrun --agents 384 --rounds 10 --workers 1
```

## 拒绝和恢复

用 128 个 Unix 客户端各发 200 KiB 后停住，64 个用 Content-Length，64 个用 chunked。每个已接收的缓冲容量达到 256 KiB，共占满 32 MiB。用带可检查副作用的命令分别试 TCP 与 Unix socket，两边均返回 503，命令均未执行。关闭占用连接后，两边均恢复 200，输出和副作用通过检查。1、4 worker 各重复三轮。官方 MCP Python SDK 1.27.2 的 TCP、Unix 和 stdio 转发复验通过。

## 普通千并发

5 对样本的复用组均执行一次命令、合并 999 次。新版峰值 RSS 比旧版高 240 KiB（6,552 → 6,792 KiB），独立执行组高 252 KiB（6,860 → 7,112 KiB）。两组整批用时比的 95% 区间均包含 1。工作区测试通过 61 项、跳过 1 项，严格 Clippy、rustdoc 和 release 构建通过。

原始样本在 Project Z 的 `dist/zrun-ingress-{slow-384-w1,slow-384-w4,slow-1000,ordinary-1000}.jsonl`。旧版和新版二进制 SHA-256 分别为 `bc0e75e489871eec8e01af731a1bc0cce71b1f4158587d8cb009e37178db357e` 和 `9d106644860537372c5b1741f3e70426a6e72b79e6ce72cfea60a9e6f7715fb5`。
