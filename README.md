# Kernary Code

Kernary Code 是一个纯终端、Rust 实现的多 Agent AI Coding Harness。它不是聊天 CLI 外壳，而是由可恢复 Kernel、Context/Memory Engine、Tool/Permission/Sandbox、MCP、Plugin、LSP 和 Agent 调度器组成的本地优先运行时。

## 安装

Windows x64 与 Linux x64 glibc：

```bash
npm install -g kernary-code
kernary --help
```

也可以从 GitHub Releases 下载便携包。`harness` 作为兼容命令保留一个迁移周期，与 `kernary` 共用状态和凭证。

## 快速开始

```bash
kernary
kernary providers
kernary models --provider opencode-go
```

Kernary 不会在未配置模型时用测试模型伪造结果。第一次进入终端后：

```text
/connect
/model
```

`/connect` 使用不回显的安全输入通道保存 Provider Key；`/model` 只选择真实或本地模型。完成后即可提交普通任务，或运行：

```bash
kernary --model openai/gpt-5.6-sol exec --json "运行测试并总结结果"
```

未配置可用模型时，普通输入与 Headless/Exec 会明确返回 `MODEL_NOT_CONFIGURED`，不会创建 Agent Mission 或模拟 Usage。

## 终端输入

- `←` / `→`：按 Unicode 字符移动光标；`Ctrl+←` / `Ctrl+→`：按单词移动；
- `Home` / `End`、`Delete` / `Backspace`：在任意输入位置编辑；
- `Ctrl+A` / `Ctrl+E`、`Ctrl+U` / `Ctrl+K` / `Ctrl+W`：兼容常用 Shell 行编辑；
- 输入 `/`：打开完整可滚动命令面板；`↑` / `↓` 选择，`Tab` 补全，`Esc` 关闭；
- `/connect ` 与 `/model `：动态列出 Provider 与模型；
- Bracketed Paste 会作为单行文本插入，不会因换行意外提交多条命令。

## 核心能力

- OpenAI Responses / OpenAI Chat / Anthropic Messages 协议与自定义中转站；
- OpenCode Go、DeepSeek、OpenRouter、Ollama 等 Provider Catalog；
- Lite / Balanced / Full / Custom 真实资源模式；
- Supervisor、Planner、Coder、Reviewer、Tester、Coordinator 等多 Agent DAG；
- Context Broker、结构化压缩、Checkpoint、Rollback 与 Prompt Canonicalization；
- MCP stdio/HTTP/SSE/OAuth、Plugin、Skill、Browser、LSP 3.18；
- Permission Rule、Sandbox hard deny、Tool Journal、Patch Preview 与安全 Undo；
- 严格非交互 `kernary exec`，适合 CI 和自动化。

## Optional Vector 硬门

未配置非空 `KERNARY_EMBEDDING_MODEL` 时，Kernary 不构造 Embedding Provider/Vector Backend，不创建向量表、目录、generation 或 job。配置有效模型后也只进入 Ready；第一次 semantic/hybrid 请求才惰性激活。

## 从源码构建

```bash
cargo build --release -p harness-cli --bins
cargo test --workspace --locked
```

Rust toolchain 版本见 `rust-toolchain.toml`。完整中文架构索引见 [TERMINAL_HARNESS_DESIGN_INDEX_ZH.md](TERMINAL_HARNESS_DESIGN_INDEX_ZH.md)。

## npm 包结构

`kernary-code` 是无原生代码的启动器，通过 npm 原生 optional dependencies 选择平台包：

- `kernary-code-win32-x64`
- `kernary-code-linux-x64-gnu`

安装脚本不会在 `postinstall` 阶段从网络下载可执行文件。

## 安全

凭证进入操作系统 Credential Store，不写入项目文件。发现安全问题请不要公开 Issue，按 [SECURITY.md](SECURITY.md) 中的方式私下报告。

## License

Apache-2.0
