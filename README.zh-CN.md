<p align="center">
  <img src="./assets/neenee-v2.png" alt="neenee logo" width="256">
</p>

<h1 align="center">妮妮</h1>

<p align="center">
  <a href="./README.md">English</a> | 简体中文
</p>

<p align="center">
  一个基于 Rust 的 AI 编码助手，提供语义化终端界面、工具调用、按需技能和有界自治追踪。
</p>

<p align="center">
  <a href="#"><img src="https://img.shields.io/badge/rust-2021%2B-orange?logo=rust" alt="Rust 2021+"></a>
  <a href="#"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License"></a>
</p>

## 特性

- **语义化终端界面** — 基于 Ratatui，支持实时状态、可展开的工具步骤、结构化 diff 展示。
- **工具调用** — 完整的 ReAct 循环，支持原生与文本回退两种工具调用协议；内置 bash、文件读写、grep、glob、网页搜索及 MCP 服务器。
- **自治追踪** — 用 `/pursue <条件>` 设定追踪目标，代理会在同一轮对话内持续工作（停止闸门）直到条件满足；用 `/repeat <cron> <提示>` 按时钟调度周期性提示。
- **持久会话** — 原子写入、上下文压缩、会话恢复与分叉。
- **技能系统** — 按需加载领域知识，或在被提及时自动注入。

## 快速开始

```bash
git clone https://github.com/ming2k/neenee.git
cd neenee
cargo run --release
```

首次启动后按 `Ctrl+M` 选择模型供应商并填入 API Key，然后直接开始对话。

## 快捷键

| 按键 | 功能 |
|------|------|
| `Enter` | 发送消息 |
| `Tab` | 接受斜杠命令 / `@path` 补全 |
| `Ctrl+M` | 打开模型选择器 |
| `Ctrl+T` | 展开 / 折叠工具详情 |
| `Ctrl+B` | 在输入框和对话流之间切换 |
| `Ctrl+C` | 复制 → 中断 → 关闭弹窗 → 清空 → 退出 |
| `Ctrl+V` | 粘贴剪贴板内容 |

## 常用命令

| 命令 | 说明 |
|------|------|
| `/pursue <条件>` | 驱动代理直到条件满足（停止闸门） |
| `/repeat <cron> <提示>` | 按 cron 表达式调度提示 |
| `/compact` | 压缩上下文以释放空间 |
| `/session list` | 浏览和恢复历史会话 |
| `/export` | 将对话导出为 Markdown |
| `/mcp` | 查看 MCP 服务器连接状态 |

## 架构

六个 crate 组成的严格分层工作区：

```
neenee-core  ←  {neenee-providers, neenee-tools, neenee-store}  ←  neenee-agent  ←  neenee-cli
```

详细架构、指南和参考文档见 [docs/](docs/)。

## 许可证

[MIT](LICENSE)
