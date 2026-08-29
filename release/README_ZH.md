# Kernary Code 便携发布包

本目录包含 Kernary Code 的纯终端 binary set、shell completion、man page 和原子安装脚本。`kernary` 是主命令，`harness` 是一个兼容周期内的字节相同 alias。

## 快速验证

```text
bin/kernary --version
bin/kernary --help
bin/kernary doctor --json
bin/kernary providers
bin/kernary models --provider opencode-go
bin/kernary exec --json "检查项目并运行测试"
bin/harness --version
```

## 首次模型设置

发布版本不会默认启用 `fake/deterministic`。未配置模型时可以浏览命令和设置，但普通任务、Team、Review 与 Headless/Exec 会返回 `MODEL_NOT_CONFIGURED`，不会伪造 Agent 完成事件。

在交互终端输入 `/` 打开完整命令面板，然后依次选择：

```text
/connect
/model
```

自定义文本模型提供商使用 `/provider add`，向导完成 URL、Key、自动模型发现与默认模型选择。`/provider switch` 切换提供商，`/model` 只切换当前提供商内的模型。

向量模型使用 `/vector setup` 单独配置：只允许一个 Provider，模型名手写且不做目录发现，保存前必须通过一次真实 Embedding 验证。`/vector clear` 删除项目配置、凭证引用与向量投影。

`/language en|zh-CN|zh-TW|ja` 可切换并持久化英语、简体中文、繁体中文和日语语言包。

产品界面把内部遥测与用户对话分层：主区只保留用户消息、Agent/Tool 活动、权限、错误与结果，详细状态仍可从 Event Log 查询。命令候选以悬浮面板显示，设置向导和 Secure Key 使用不同输入状态；顶部状态条会响应终端宽度显示 Git 分支、模型、Context 进度和运行中的 Agent。

方向键选择候选，Tab 补全，PgUp/PgDn 回看对话；API Key 通过独立 Secure Lane 输入并进入 OS Credential Store。终端编辑支持左右光标、Home/End、Delete/Backspace、Ctrl+A/E/U/K/W、Ctrl+Left/Right 和安全 Bracketed Paste。

`doctor`、`--help`、completion 和 man 生成不会创建项目 `.harness` 状态。

## Adaptive Agent Team

`/agents compact|verbose|tree` 可查看 15 个内置 Agent；它们默认 Sleeping。高保障任务可运行：

```text
/team adaptive 2 release secure auth service with performance benchmark
```

固定骨架为 Requirements + Explorer → Architect → Planner → Coder workers → Reviewer → Tester；目标命中安全、性能、发布类别时，分别增加 Security Auditor、Performance Engineer、Release Manager 证据门。所有 Agent 使用独立工作 Context、最小 Tool 视图和有界预算，Staffing Router 只读取结构化能力元数据。

## 模型目录

普通模型列表只读取内置快照与已有缓存，不访问网络：

```text
kernary models
kernary models --provider anthropic --json
```

只有显式指定单个 Provider 才刷新远端目录：

```text
kernary models --refresh anthropic
kernary models --refresh ollama
```

刷新缓存位于项目 `.harness/provider-models-v1.json`，不包含 API Key。OpenCode 等多协议 Provider 的未知模型只显示为 discovered/unroutable，不会根据名称猜测协议。

自定义中转站可从 `examples/kernary.providers.toml` 复制到项目根目录 `kernary.providers.toml`；示例只含 credential reference，不含真实 Key。

## Mode、Config 与 Permission

把 `examples/kernary.toml` 复制到项目根目录即可设置 Project 层。运行中可使用 `/config` 查看每项来源，使用 `/mode`、`/settings` 和 `/permissions` 写 Session 或 Runtime 层。权限分为 `manual`、`edit`、`auto`、`full`、`bypass`；TUI 使用 Shift+Tab 在前四级循环，最高 `bypass` 必须显式确认，任何等级都不能绕过 denied roots、Sandbox allowlist 或项目边界。

## 系统级 Sandbox

默认 `workspace-write`。Windows 使用受限 Primary Token、项目 capability ACL、私有 Desktop 与 Job Object；Linux 使用 `bubblewrap` mount/user/network namespace。项目外、`.git` 与 `.harness` 写入会被操作系统拒绝；`read-only` 只保留隔离临时目录写权限。Linux 未安装可信 `bwrap` 时命令 fail closed。

```text
/sandbox
/sandbox read-only
/sandbox workspace-write
/sandbox network-on
/sandbox network-off
/sandbox danger-full-access
```

网络默认关闭；Windows 兼容后端会明确标注环境级断网不等于 WFP。`network-on` 与 `danger-full-access` 都有独立确认；非交互启动分别使用 `--sandbox-network-access` 和 `--sandbox danger-full-access --confirm-dangerous-sandbox`。

## Session 与私有辅助状态

`kernary` 每次创建新 Session；`kernary -c` 继续当前项目最近会话，`kernary -r [id-or-title]` 从当前项目选择恢复。会话内使用 `/session`、`/session new`、`/session switch` 和 `/session rename`。标题由第一条有效对话本地生成，完整 Transcript 不随 Context 压缩删除。

项目私有指令位于 `.harness/agent.md`，不存在时才读取全局 `~/.kernary/agent.md`；使用 `/agentmd` 管理。向量配置位于 `.harness/vector.toml`，Memory/Repository/Vector SQLite 也全部位于 `.harness/`。Kernary 自动写入 `.git/info/exclude`，这些辅助文件默认不会进入 Git。

细粒度规则可从 `examples/kernary.permissions.toml` 开始，也可使用：

```text
/permissions rule add allow read ./src/**
/permissions rule add ask write ./config/**
/permissions rule add deny read ~/.ssh/**
/permissions rules
/permissions rule remove <rule-id>
```

## MCP

把 `examples/kernary.mcp.toml` 复制到项目根目录，或使用 `/mcp add-stdio`、`/mcp add-http`、`/mcp enable`、`/mcp disable`、`/mcp remove` 原子更新。Add 只写 metadata，不会隐式连接；API Key/OAuth token 不写 TOML。

## 非交互自动化

`kernary exec` 不会请求交互式 Key 或审批，支持单 JSON document、quiet 和原子输出：

```text
kernary exec --json "运行测试"
kernary exec --json --quiet --output result.json "审查项目"
```

已有 output 默认拒绝覆盖；必须显式加 `--force`。

## LSP Bridge

把 `examples/kernary.lsp.toml` 复制到项目根目录，填写本机 Language Server 的绝对路径。配置加载不会启动进程；只有 `/lsp start`、具体 Slash 查询或经过审批的 on-demand `lsp.*` Agent Tool 才 lazy spawn。Bridge 只读，任何 `workspace/applyEdit` 都会被拒绝；LSP 输出按 Untrusted 数据进入 Context。

终端与 Tool 使用 1-based Unicode 字符列；Kernary 会转换为 Server 协商的 UTF-8/UTF-16/UTF-32 坐标。LSP symbols/diagnostics 以文件 SHA-256 绑定到 Repository Index，文件改变后旧事实不会继续参与排名。

Rename/CodeAction 不会直接写源码，只生成 `.harness/lsp-previews` 中的 hash-anchored Preview。`/lsp apply` 与 `/lsp undo` 分别要求 WorkspacePatch 审批，并通过排序 FileLease、PatchStore 子记录和可恢复 PatchSet journal 执行。

## 安装与回滚

Windows PowerShell：

```powershell
.\install.ps1
.\install.ps1 -Rollback
```

Linux/macOS：

```bash
./install.sh
./install.sh --rollback
```

安装脚本先把新二进制复制到目标目录并执行 `--version`，验证后才替换当前版本。旧版本进入目标目录的 `rollback/`，显式 rollback 会与最近一版交换。

## 项目状态备份

```text
kernary maintenance backup --output <absolute-or-project-relative-directory>
kernary maintenance verify <backup-directory>
kernary maintenance restore <backup-directory> --force
```

Restore 前自动生成 `pre-restore-*` recovery point。备份不包含 OS Credential Store、Browser Profile/Download、cache 或临时文件。

## Optional Vector

未设置或留空 `KERNARY_EMBEDDING_MODEL`（legacy fallback：`HARNESS_EMBEDDING_MODEL`）时，Kernary 不构造 Embedding Provider/Vector Backend，不创建向量表、目录、generation 或 job。设置有效模型后也只进入 Ready；首次 semantic/hybrid 请求才惰性激活。

## 命令与状态兼容

- `kernary` 与 `harness` 读取同一个 `.harness` 项目状态目录；
- 两者使用同一个 `dev.openai.harness` OS Credential Service；
- `KERNARY_*` 优先，`HARNESS_*` 作为兼容 fallback；
- Windows 一个周期内继续安装到旧默认目录 `Programs/Harness/bin`，避免现有 PATH 失效；
- Install/Rollback 会把 `kernary` 与 `harness` 当作一个原子 binary set。
