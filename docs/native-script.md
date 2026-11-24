# .zrun 脚本

`zrun FILE.zrun` 使用配置的 shell 运行普通脚本，并转交其位置参数。在首行写下 `# zrun:1` 可启用 Linux 上的原生任务编排。

可执行的原生脚本可以用 `#!/usr/bin/env zrun` 作为首行，把 `# zrun:1` 放在第二行。赋予脚本执行权限后，可直接运行 `./FILE.zrun`。

```sh
# zrun:1
async prepare <<COMMAND
for src in a b; do printf '%s\n' "$src"; done
COMMAND
async inspect {"script":"printf '%s\n' inspection","reuse":{"scope":"inspect","input_revision":"v1","ttl_ms":5000}}
await inspect
shell <<COMMAND
printf 'inspection completed\n'
COMMAND
await prepare
```

`async 名称` 提交任务后继续处理下一条指令。`await 名称` 等待并输出该任务，其他已提交任务可以继续运行。`await 名称1 名称2` 按列出的顺序等待并输出多个任务。`await *` 按提交顺序等待当前所有待收取的任务，队列为空时直接继续。`shell` 提交一个任务并立即等待。三种操作共用所选执行队列和授权策略。脚本退出码取第一个已等待任务的非零退出码。任务名可在相应的 `await` 后复用。

shell 块中的语法由配置的 shell 解释。各块独立运行，并可通过文件或其他明确的输入输出交换数据。JSON 请求可以指定 `cwd`、超时及复用键。整个原生脚本会在执行前校验任务名、等待关系和输入长度。原生模式最多同时保留 128 个待等待任务。普通 shell 模式沿用所选 shell 的变量、函数、位置参数和退出行为。

设置 `ZRUN_SOCKET` 时，任务通过 Unix socket 提交到同一个 zrun 服务，可以与其他脚本、CLI 和 MCP 客户端共享执行队列、在途合并及缓存。授权由服务执行。相对工作目录从脚本进程的当前目录解析。连接失败会作为该任务的失败返回。


`zrun FILE.zrun ARG...` 将 UTF-8 参数原样传给原生脚本的 shell 块，供 `$1`、`$2` 和 `"$@"` 使用。JSON 请求中非空的 `args` 数组覆盖该块的参数。最多 128 个参数，合计不超过 8 KiB。zguard 检查参数，复用键也包含参数。现有的 zplugin 授权接口无法接收参数，因此启用该接口时，有参数的任务会被拒绝。
