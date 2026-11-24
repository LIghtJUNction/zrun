# zrun 命令服务

普通 `.zrun` 文件使用配置的 shell。参见[原生异步 .zrun 脚本](native-script.md)。

在 Unix 上，`zrun` 依次查找 `ZRUN_SHELL`、`$XDG_CONFIG_HOME/zrun/shell`（默认 `~/.config/zrun/shell`）、`SHELL` 和 `/bin/sh`。配置文件写一行后端 shell 的绝对路径，例如 `/usr/bin/zsh`。这让 zrun 作为登录 shell 时也能使用原有后端。普通 shell 参数也可以直接传入，例如 `zrun -c 'cd /tmp; pwd'`。这种入口继承终端与 shell 的变量、作业控制和退出语义。受管执行从 `zrun run`、`zrun batch` 和 MCP 进入。

## 本地 socket

在 Linux 上启动一个进程，供多个 Project Z 项目和 agent 共用。默认监听 `$XDG_RUNTIME_DIR/zrun/mcp.sock`（`--unix` 或 `--socket`）。也可传入 `--unix PATH`。目录需要由服务用户持有且权限为 `0700`。服务以 `0600` 创建 socket，正常退出或收到 SIGINT、SIGTERM 时删除自己的 socket。

```sh
zrun mcp --unix
```

默认路径要求 `XDG_RUNTIME_DIR` 为同一用户持有的绝对路径且权限为 `0700`。自定义路径的父目录应由调用方以相同权限预先创建。已有 socket 会使启动失败，服务会在退出时按文件身份清理自己创建的 socket。参见 [Project Z Unix socket 规范](https://github.com/LIghtJUNction/project-z/blob/main/docs/unix-sockets.md)。

客户端通过 `ZRUN_SOCKET` 连接到它。MCP 客户端可以直接通过 Unix socket 发送 HTTP，也可以启动 `zrun mcp --stdio` 并给它设置 `ZRUN_SOCKET`。stdio 进程会转发消息到同一个服务。

```sh
export ZRUN_SOCKET="$XDG_RUNTIME_DIR/zrun/mcp.sock"
zrun run 'git status --short'
printf '%s\n' '{"script":"printf first"}' '{"script":"printf second"}' | zrun batch
```

其他项目接入时，与 TCP 使用相同的 **MCP Streamable HTTP** 消息。Unix 客户端向 `POST /mcp` 发送一个 JSON-RPC 对象，`Host` 使用 `zrun` 或 `localhost`。请求包含 `Content-Type: application/json` 和 `Accept: application/json, text/event-stream`。TCP 和 Unix 监听器在同一进程启动时共用队列与缓存。

```sh
curl --unix-socket "$ZRUN_SOCKET" http://zrun/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  --data-binary '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example","version":"1"}}}'
```

MCP 客户端依次使用 `initialize`、`notifications/initialized` 和 `tools/list`。TCP 通过 `zrun mcp --http 127.0.0.1:PORT` 启动，需要预置至少 32 个字符的 `ZRUN_MCP_TOKEN`，客户端带 `Authorization: Bearer <token>`。服务支持同时指定 `--http` 和 `--unix`。TCP 监听限制在 loopback，与 Unix socket 一样用于可信本地客户端。

stdio 采用每行一条 JSON-RPC。HTTP 每次 POST 处理一条消息，请求支持 Content-Length 和 chunked，响应使用 JSON，通知返回 HTTP 202。独立 GET 事件流返回 405。协议版本为 2025-11-25，兼容 2024-11-05、2025-03-26 和 2025-06-18 的初始化流程。 [MCP 传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

## 命令和结果

`shell_run` 接收一个命令对象。`shell_batch` 接收 `{"commands":[...]}`，上限 16 条。命令在共享队列内并发执行，MCP 批量结果按输入顺序返回。CLI 的 JSONL 结果带有从零起算的 `index`，本地批量流按完成顺序输出。

```json
{
  "script": "git status --short",
  "cwd": "/home/user/repo",
  "timeout_ms": 10000,
  "reuse": {
    "scope": "repo-status",
    "input_revision": "caller-computed-worktree-revision",
    "ttl_ms": 5000
  }
}
```

`script` 由服务启动时选择的 shell 解释。`cwd` 省略时使用服务目录，通过 CLI 转发时使用客户端目录。受管命令标准输入为空，stdout 和 stderr 分别限制为 64 KiB。超出上限后继续排空管道，并在结果中标记 `truncated`。输出对象统一使用 `{"encoding":"utf8","data":"..."}` 或 RFC 4648 标准 Base64。Linux 命令使用各自的进程组，超时时终止组内进程，返回 124，同时保留已捕获的输出。

结果的 `disposition` 说明来源：

- `executed` 由 zrun 启动了命令。
- `coalesced` 等待同一份在途工作。
- `cached` 使用有效的已完成结果。
- `rejected` 命令未进入执行队列。

省略 `reuse` 时，每次调用分别执行。带 `reuse` 的命令由调用方声明可跨请求复用，并提供覆盖实际读入内容的版本标识。身份由同一服务内的 shell、脚本、规范化目录、超时及复用字段共同决定。`ttl_ms: 0` 只合并在途请求。TTL 大于零时，服务在内存中保存成功且未截断的结果。修改文件、部署、安装、查询实时状态等工作一般需要独立执行，只有调用方能约束其语义。缓存随进程退出清除。 MCP 服务在同一结果的多个响应同时发送时共享输出编码，每条响应各自保有请求 ID 和结果状态。

默认使用单 worker 执行器，直接处理 socket 和命令描述符的就绪事件。设置 `ZRUN_WORKERS=2` 至 `16` 可使用多 worker 候选。每个服务最多同时运行 32 个命令，最多登记 4,096 个待交付请求。执行中及排队的命令输入合计上限 16 MiB。缓存最多 1,024 项，记录的键和输出缓冲区合计上限 16 MiB。socket 接入最多 2,048 个活动连接。 TCP 和 Unix socket 共用 32 MiB 的 HTTP 接收缓冲预算。请求体上限为 256 KiB。接收缓冲随已收到的数据增长，预算耗尽时返回 HTTP 503，且当次请求未进入命令队列。 MCP 回复共用 32 MiB 的结果编码预算。编码空间不足时，服务暂存已完成的原始结果，在可用空间恢复后交付原有结果。原始抓取缓冲共用独立的 128 MiB 预算。每个请求在执行前一次预留整批空间，空间不足时异步排队。命令完成后按实际占用缩减预留，完成编码后释放。内核 socket 缓冲另计。

## Project Z 集成

- `zrun --zmem interactive run 'cat /proc/self/cgroup'` 用 zmem 启动整个 zrun 实例。常驻服务也可用 `zrun --zmem background mcp --unix PATH`。子命令继承服务的 cgroup，避免每条工作重复创建 zmem scope。
- `ZRUN_POLICY=/path/to/policy.json` 配置 zguard。可用字段为 `shell`（`allow`、`deny` 或 `require_approval`）、`max_calls` 和 `max_payload_bytes`。每次提交都检查，包括读缓存。`require_approval` 当前返回拒绝。
- `ZRUN_PLUGIN=/path/to/plugin.so` 加载可信的 zplugin 原生准入插件。接口 ID 为 `dev.project-z.zrun.admission`，由 `zrun-plugin-api` 提供。版本 2 接收完整的位置参数，每个 UTF-8 参数以 NUL 结尾。版本 1 仍受支持，只接受无位置参数的任务。插件在独立线程用有界队列检查规范化后的调用。插件返回拒绝或错误时，服务不会执行命令或读取缓存。
- `zrun::io` 可在 Rust 项目中直接使用，包含独立线程的 `Reactor`、由 `Runtime::block_on_with_io` 驱动的 `Poller`、TCP 流与 Unix domain socket。应用可以采用上面的 MCP 协议，作为跨语言或跨项目的统一连接格式。

受管命令继承服务账号的系统权限。socket 的文件权限和 TCP 密钥控制入口，zguard 与插件控制准入。需要隔离不同信任域时，为各域运行独立服务和相应的 OS 权限边界。

## 验证

```sh
cargo test --locked --workspace --all-features --all-targets
cargo build --locked --release -p zrun-cli
python3 scripts/agent-load.py --binary target/release/zrun --agents 1000 --rounds 5
```

负载程序对同一命令比较独立执行和集中复用。每次校验所有请求及输出，并采样服务进程 RSS、线程和直接子进程。Tokio 的运行时比较仍使用独立的 [benchmark 协议](benchmark-protocol.md)。

[首次 Linux 验收与 1,000 并发客户端原始口径](measurements/agent-shell-linux-20261124.md)

[单线程 I/O 与新版命令服务的验证](measurements/p2-tcp-linux-20261124.md)

[大输出与慢客户端的内存实测](measurements/mcp-output-linux-20261124.md)

[HTTP 请求缓冲与过载恢复实测](measurements/mcp-ingress-linux-20261124.md)

[不同命令输出的出站背压实测](measurements/mcp-egress-linux-20261124.md)

[批量命令的原始结果预算实测](measurements/mcp-capture-linux-20261124.md)


Unix socket 的 HTTP/1.1 客户端可以发送 `Connection: keep-alive`，并在读完当前响应后在同一连接发送下一条请求。使用 `Content-Length` 定界，请求不可流水线发送。每条请求重新检查 Host、协议版本和授权，每条连接最多 64 次请求。响应头返回 `Connection: close` 时应重新连接；连接中断后，只有在确定请求可以安全重复时才应重试。

`zrun batch` 的远程模式和 `mcp --stdio` 的转发模式会在服务端确认后复用顺序连接。旧服务端返回 `Connection: close` 时，下一条请求会新建连接。上传或等待结果中断时，客户端报告错误并丢弃该连接。原生脚本中并行的独立 `async` 任务继续独立收取结果。
