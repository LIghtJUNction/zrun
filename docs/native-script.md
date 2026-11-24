# .zrun 脚本

`zrun FILE.zrun` 使用配置的 shell 运行普通脚本，并转交其位置参数。在首行写下 `# zrun:1` 可启用 Linux 上的原生任务编排。

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

`async 名称` 提交任务后继续处理下一条指令。`await 名称` 等待并输出该任务，其他已提交任务可以继续运行。`shell` 提交一个任务并立即等待。三种操作共用同一个本地执行队列和授权策略。脚本退出码取第一个已等待任务的非零退出码。任务名可在相应的 `await` 后复用。

shell 块中的语法由配置的 shell 解释。各块独立运行，并可通过文件或其他明确的输入输出交换数据。JSON 请求可以指定 `cwd`、超时及复用键。整个原生脚本会在执行前校验任务名、等待关系和输入长度。原生模式最多同时保留 128 个待等待任务。普通 shell 模式沿用所选 shell 的变量、函数、位置参数和退出行为。

原生模式当前使用本地 broker。跨进程共享同一队列的统一入口仍是 zrun 的 Unix socket、MCP 和现有批处理接口。
