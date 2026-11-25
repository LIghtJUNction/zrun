# zrun 命令服务

## Linux 终端

在 Linux 终端运行 `zrun`、`zrun -i` 或 `zrun -l` 会看到 `zrun> `。zrun 进程读取命令，解析参数，按 `PATH` 启动程序，并把终端交给前台管道。交互会话中的 `$0` 为 `zrun`，`$SHELL` 是当前 zrun 可执行文件的路径。用 `zrun -c '命令'`、`zrun FILE` 或 `zrun < FILE` 可运行同一套语法；文件开头的 shebang 可以指定解释器。

```sh
cd /tmp
label='two words'
export label
async first -- printf '<%s>\n' "$label"
pwd
await first
```

这段输入会先打印工作目录，再打印 `<two words>`。赋值保存在当前会话，`export` 让后续子进程继承指定变量。每个命名任务在提交时取得目录和变量的副本，标准输入为 `/dev/null`。`await 名称…` 按指定顺序交付已捕获的标准输出和错误输出，`await *` 按提交顺序等待，`await --ready` 优先交付已完成的任务。最多同时保留 128 个待收取任务，每个输出流最多保留 64 KiB。`$?` 保存上一条命令的退出码。对前台程序按 Ctrl-C 会发送中断，Ctrl-D 或 `exit` 结束当前会话。

原生语法支持单双引号、反斜杠转义、变量与位置参数、管道、`&&`、`||` 及常用文件重定向。它按输入行解析；通配符展开、命令替换、函数、循环、多行引号和交互历史尚未实现。可用 `zrun --shell -c '…'` 调用配置的兼容 shell，或在 zrun 提示符输入 `shell -- 命令`。兼容 shell 的路径依次取自 `ZRUN_SHELL`、`$XDG_CONFIG_HOME/zrun/shell`（默认 `~/.config/zrun/shell`）、启动环境的 `SHELL` 和 `/bin/sh`。

命名任务也可通过专用的 `# zrun:1` 文件提交给受管队列。该格式支持显式输入版本、超时及跨客户端复用。命令服务另有 `zrun run`、`zrun batch` 和 MCP 入口。详见[受管脚本语法](native-script.md)。

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

`shell_run` 接收一个命令对象。`shell_batch` 接收 `{"commands":[...]}`，上限 16 条。命令在共享队列内并发执行，MCP 批量结果按输入顺序返回。CLI 的 JSONL 结果带有从零起算的 `index`，本地批量流按完成顺序逐行输出，调用方可以在其他命令仍运行时读取结果。远程批量流默认按输入顺序输出。使用 `zrun batch --ready` 时，任何已完成的 MCP 批次都可先逐行输出并腾出请求窗口。单批内保留输入顺序，`index` 仍是原始输入的下标。对互相独立的大批量命令，可设置 `ZRUN_BATCH_WINDOW=2` 至 `8` 并行提交多个 MCP 请求，默认值为 `1`。窗口中的请求可能以任意顺序执行。CLI 会在首次请求前校验全部 JSONL 输入及编码后的单条请求大小。预编码缓存最多保留 2 MiB，其余批次在各自发送窗口到达时重新编码。客户端遇到传输错误时退出，窗口中已提交的命令仍可能执行。

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

默认使用单 worker 执行器，直接处理 socket 和命令描述符的就绪事件。设置 `ZRUN_WORKERS=2` 至 `16` 可使用多 worker 候选。多 worker 默认使用独立 I/O 线程。设置 `ZRUN_IO_DRIVER=worker` 可由首个执行 worker 同时驱动 I/O 和定时器。每个服务最多同时运行 32 个命令，最多登记 4,096 个待交付请求。执行中及排队的命令输入合计上限 16 MiB。缓存最多 1,024 项，记录的键和输出缓冲区合计上限 16 MiB。socket 接入最多 2,048 个活动连接。 TCP 和 Unix socket 共用 32 MiB 的 HTTP 接收缓冲预算。请求体上限为 256 KiB。接收缓冲随已收到的数据增长，预算耗尽时返回 HTTP 503，且当次请求未进入命令队列。 MCP 回复共用 32 MiB 的结果编码预算。编码空间不足时，服务暂存已完成的原始结果，在可用空间恢复后交付原有结果。原始抓取缓冲共用独立的 128 MiB 预算。每个请求在执行前一次预留整批空间，空间不足时异步排队。命令完成后按实际占用缩减预留，完成编码后释放。内核 socket 缓冲另计。

## 可选 Valkey 二级缓存

默认只使用进程内合并与结果缓存。显式设置下列变量可启用二级缓存：

```sh
export ZRUN_VALKEY_SOCKET=/absolute/path/to/valkey.sock
export ZRUN_VALKEY_DOMAIN=team_a
export ZRUN_VALKEY_TOOL_REVISION=toolchain-2026-11
```

对通过授权且 `reuse.ttl_ms > 0` 的调用，zrun 本地未命中后最多等待 50 ms 查询 Valkey。同进程的相同在途请求共用一次查询或执行。命令成功且输出完整时用 `SET … PX` 写入，写入失败不影响命令结果。连接中断、超时或无效值按未命中处理，并在同一 broker 内暂停访问 Valkey 一秒。普通未命中不会触发退避。每个 broker 命令 worker 最多保留一个空闲 Valkey 连接。连接只有在完整收取响应后才可复用。二级命中回填本地缓存时使用剩余有效期。跨进程共享的是已完成结果。

键以前缀 `zrun:v1:<domain>:` 隔离，摘要包含脚本、位置参数、解析后的目录、超时、原有复用参数、zrun 版本、shell 路径、继承环境和工具版本。更新工具、shell 或会影响结果的执行配置时应更新 `ZRUN_VALKEY_TOOL_REVISION`。工作树与文件输入仍由调用方通过 `input_revision` 标识。选择同一信任域的进程必须信任 Valkey 中的值；使用隔离的 socket 或 Valkey 访问控制限制不同信任域的读写权限。只支持本机 Unix socket。

可用 `python3 scripts/valkey-smoke.py /path/to/zrun` 运行真实 Valkey 验收。该测试启动独立临时实例，需要本机有 `valkey-server` 和 `valkey-cli`。

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


Unix socket 和 TCP 的 HTTP/1.1 客户端可以发送 `Connection: keep-alive`，并在读完当前响应后在同一连接发送下一条请求。使用 `Content-Length` 定界，请求不可流水线发送。每条请求重新检查 Host、协议版本和授权，TCP 客户端每次都要发送 Bearer 令牌。每条连接最多 64 次请求。响应头返回 `Connection: close` 时应重新连接；连接中断后，只有在确定请求可以安全重复时才应重试。

`zrun batch` 的远程模式和 `mcp --stdio` 的转发模式会在服务端确认后复用顺序连接。旧服务端返回 `Connection: close` 时，下一条请求会新建连接。上传或等待结果中断时，客户端报告错误并丢弃该连接。远程 `.zrun` 脚本也会复用空闲连接，单个脚本最多保留 8 条。并行 `async` 任务各自使用连接并独立收取结果。

Linux 回环验证使用同一 release 二进制、四 worker 的 broker 和固定 CPU 的客户端，交替执行 12 对独立的 512 条 `tools/list` 请求。每条请求校验 HTTP 状态、响应 ID 和工具列表。显式复用时建立 8 条连接，逐条关闭时建立 512 条。耗时中位数分别为 65.5 ms 和 83.7 ms，配对比值为 0.783，95% 区间为 [0.761, 0.802]。该测试只覆盖顺序协议请求，不包含命令执行时间。

在四 worker 的 Linux Unix socket broker 上，用同一 release 二进制交替测试 12 对批处理。每批输入 96 条命令，请求窗口为 2。首条命令等待 150 ms，其余命令各等待 3 ms。所有输出、退出码和原始下标均已校验。`--ready` 将首条结果中位数从 154.1 ms 降到 10.8 ms，比值为 0.070 [0.059, 0.076]。整批从 172.2 ms 降到 154.1 ms，比值为 0.895 [0.892, 0.899]。区间来自 5,000 次配对 bootstrap。结果适用于该快慢混合负载。
