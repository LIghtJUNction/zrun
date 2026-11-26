# Zrun for Omarchy

状态栏显示当前在线的本地 zrun MCP broker 成功启动了多少命令。点击查看实际启动、避免重复执行、内存和 Valkey 命中、在途合并、处理中和排队数量。插件每两秒读取一次 `zrun --stats`。

## 安装

从 zrun 仓库根目录执行：

```sh
cargo install --locked --path crates/zrun-cli --root "$HOME/.local"
plugin_dir="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/lightjunction.zrun"
mkdir -p "$plugin_dir"
cp -R packaging/omarchy/. "$plugin_dir/"
omarchy plugin validate "$plugin_dir"
omarchy plugin enable lightjunction.zrun
```

插件优先读取 `~/.local/bin/zrun`，其后从 Shell 的 PATH 查找。也可在 Omarchy Shell 的启动环境中设置 `ZRUN_STATS_BIN` 指定其他可执行文件。

将 agent 的 zrun MCP 服务端命令指向新版 zrun，例如 `~/.local/bin/zrun mcp --stdio`，然后重新连接该 MCP 服务。共享 Unix/TCP broker 也需用新版启动。已有服务进程结束时会从汇总移除。计数定义见 [实时统计](../../docs/agent-shell.md#本地实时统计)。

关闭插件可运行 `omarchy plugin disable lightjunction.zrun`。

## 使用共享 MCP 服务

需要让状态栏持续显示统计时，可安装本目录的用户服务。已经使用共享 zrun 服务时，继续使用已有服务和 socket。

```sh
mkdir -p ~/.config/systemd/user
install -m644 packaging/omarchy/zrun-mcp.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now zrun-mcp.service
```

例如，把新 Codex 会话的 zrun MCP 入口接到这个服务：

```sh
codex mcp add zrun --env ZRUN_SOCKET="$XDG_RUNTIME_DIR/zrun/mcp.sock" -- "$HOME/.local/bin/zrun" mcp --stdio
```

## 显示与覆盖范围

明暗外观和界面语言跟随 Omarchy。插件提供简体中文、繁體中文、英语、日语、法语、德语和西班牙语，其他语言回退到英语。

复用条按实际次数显示「复用请求 / 已有执行或复用结果的请求」。待执行、校验失败和未成功启动的请求不进入该比例。多个服务在线时汇总各服务启动以来的计数。

已有 agent 可接入同一个 Unix broker。批处理也可设置 `ZRUN_SOCKET="$XDG_RUNTIME_DIR/zrun/mcp.sock"`，让 `zrun batch` 发送到该 broker。缓存和在途合并需要调用方对可复用任务提供准确的 `input_revision`，例如依赖锁文件检查或固定输入的静态分析。部署、写文件和依赖实时状态的任务应独立执行。
