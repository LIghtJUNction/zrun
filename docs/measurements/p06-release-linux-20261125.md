# p0.6 Linux 发行验收

Git 标签 `v0.1.0-p0.6` 指向 `73ae04fe4b0b6dfacea7bd9bdc6513db29380e5a`。源码及 crates.io 投影均通过全 feature、全 target 工作区测试，源码通过严格 Clippy。GitHub 下载的 Linux x86_64 二进制通过 SHA-256 校验。crates.io 锁定安装的 CLI 和 GitHub 二进制均完成普通 shebang、连字符脚本名和 Unix socket 远程 `await --ready` 实测。

crates.io 安装的 CLI 使用 `-zrun` 作为 argv[0] 启动真实 Linux PTY。隔离配置目录的 `zrun/shell` 指向 `/usr/bin/zsh`，且 `SHELL` 指回 zrun。在该 PTY 中确认了登录与交互标志、stdin TTY、后台进程等待和退出码 37。采用隔离的 zsh 启动文件，账户登录 shell 仍为 `/usr/bin/zsh`。此次测试覆盖后端委托与 PTY 行为。

## 调度器长测

从同一干净标签构建的 Linux release 基准 SHA-256 为 `387ef865c17a65ab72ffd56a46f5b8ed374d22377d4bb051239d019ecc25fc1d`。Rust 1.100.0，Intel Core i9-12900HX，四 worker 固定在逻辑 CPU 18–19，与已有长测共享这组 CPU。每轮提交 10,000 个任务，每任务自唤醒 16 次；每轮校验返回值和零活跃任务。用户服务 `zrun-soak-p06-w4.service` 于 2026-11-25 01:16:58 UTC+8 启动，目标运行 86,400 秒，每 30 秒由外部控制进程采样 RSS。

首次 30 秒定期采样看到运行进程完成 4,100 万个任务，活跃任务数为零。完整时长与内存趋势等待最终事件和进程正常退出。此长测覆盖调度器生命周期，TCP、MCP 和 Tokio 性能门槛分别验收。原始记录位于 `dist/zrun-soak-24h-p06-w4/`。
