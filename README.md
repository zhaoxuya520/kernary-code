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

添加自定义 OpenAI-compatible 中转站时使用：

```text
/provider add
```

向导依次要求 API Base URL、Secure Key，随后自动请求同源 `/models`，让用户选择默认模型，并原子保存到 `kernary.providers.toml`。切换分为两层：

```text
/provider switch       # 切换提供商并采用其默认模型
/model                  # 只列当前提供商的模型
/model <model-id>       # 当前提供商内快速切换
/provider remove <id>   # 删除项目级 Provider 与凭证引用
```

单一向量模型提供商使用独立向导：

```text
/vector setup
```

依次输入 Embedding Base URL、Secure Key 和手写模型名。Kernary 不拉取向量模型目录，只发送一次真实 Embedding 请求；成功后记录返回维度并保存 `kernary.vector.toml`。使用 `/vector clear` 可恢复 lexical-only。

界面语言支持高度定制的命令目录、快捷键提示和设置向导：

```text
/language en
/language zh-CN
/language zh-TW
/language ja
```

## 产品级终端界面

交互界面采用 transcript-first 信息架构：顶部只保留项目、Git 分支、模型、模式、Context 进度和运行中的 Agent；主区域只显示用户消息、Agent 工作、工具、权限、错误与最终结果。`SystemReady`、`Usage`、`Plan`、`Context` 等内部遥测继续进入可审计 Event Log，但不再淹没主对话。

- 命令面板以悬浮层出现，不改变对话区高度；
- 输入框区分普通输入、设置向导与 Secure Key 三种语义状态；
- `PgUp` / `PgDn` 回看长对话，输入与新输出保持在固定位置；
- 宽屏显示完整运行信息，窄屏自动收缩模型名、进度条和次要状态；
- 颜色使用语义 token 且继承终端默认前景/背景，`--no-color` 保留完整可读性。

`/connect` 使用不回显的安全输入通道保存 Provider Key；`/model` 只选择真实或本地模型。完成后即可提交普通任务，或运行：

```bash
kernary --model openai/gpt-5.6-sol exec --json "运行测试并总结结果"
```

未配置可用模型时，普通输入与 Headless/Exec 会明确返回 `MODEL_NOT_CONFIGURED`，不会创建 Agent Mission 或模拟 Usage。

## 内置 Agent 与 Adaptive 工作流

Kernary 内置 15 个按职责、上下文、工具权限和证据责任划分的 Agent；空闲时全部 Sleeping。除原有 Staffing Router、Coordinator、Planner、Coder、Reviewer、Tester、Debugger、Researcher、Merge Agent 外，还包含：

- Requirements Analyst：范围、非目标、歧义和确定性验收标准；
- Explorer：隔离主会话的只读代码库入口、符号、依赖和数据流探索；
- Architect：边界、契约、失败模式、迁移兼容和 ADR；
- Security Auditor：独立威胁模型、漏洞和供应链证据门；
- Performance Engineer：基线、负载、瓶颈与回归阈值证据门；
- Release Manager：版本、产物、校验和与回滚就绪证据门。

高保障任务使用能力路由工作流：

```text
/team adaptive 2 <objective>
```

Requirements 与 Explorer 首波并行，Architect 和 Planner 依赖其压缩结果；Coder 完成后 Reviewer 与命中的 Security/Performance 审计并行，Tester 汇总全部证据，发布类任务最后再经过 Release Manager。Security、Performance 和 Release 由目标关键词确定性激活，不会为无关任务凑数。Staffing Router 只读取结构化 capability/capacity/cost 元数据，不把完整 Agent 说明注入主会话。

## 终端输入

- `←` / `→`：按 Unicode 字符移动光标；`Ctrl+←` / `Ctrl+→`：按单词移动；
- `Home` / `End`、`Delete` / `Backspace`：在任意输入位置编辑；
- `Ctrl+A` / `Ctrl+E`、`Ctrl+U` / `Ctrl+K` / `Ctrl+W`：兼容常用 Shell 行编辑；
- 输入 `/`：打开完整可滚动命令面板；`↑` / `↓` 选择，`Tab` 补全，`Esc` 关闭；
- `PgUp` / `PgDn`：滚动主对话，不干扰输入历史和命令候选；
- `/connect ` 与 `/model `：动态列出 Provider 与模型；
- Bracketed Paste 会作为单行文本插入，不会因换行意外提交多条命令。

## 核心能力

- OpenAI Responses / OpenAI Chat / Anthropic Messages 协议与自定义中转站；
- OpenCode Go、DeepSeek、OpenRouter、Ollama 等 Provider Catalog；
- Lite / Balanced / Full / Custom 真实资源模式；
- 15 个最小权限内置 Agent、能力路由 Adaptive DAG 与独立 Evidence Gate；
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

Rust toolchain 版本见 `rust-toolchain.toml`。

## npm 包结构

`kernary-code` 是无原生代码的启动器，通过 npm 原生 optional dependencies 选择平台包：

- `kernary-code-win32-x64`
- `kernary-code-linux-x64-gnu`

安装脚本不会在 `postinstall` 阶段从网络下载可执行文件。

## 安全

凭证进入操作系统 Credential Store，不写入项目文件。发现安全问题请不要公开 Issue，按 [SECURITY.md](SECURITY.md) 中的方式私下报告。

## License

Apache-2.0
