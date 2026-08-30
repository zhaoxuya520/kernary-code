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

自定义文本模型提供商使用 `/provider add`，向导完成厂商命名、协议选择、URL、Key、自动模型发现与默认模型选择。文本 Provider 和默认模型属于用户全局配置：`/provider switch` 每次打开都重新读取合并目录，另一个窗口刚添加的 Provider 也会出现并按需注册；`/provider switch` 或 `/model` 成功后原子更新全局 `providers.toml` 与 `model.json`，新窗口、新项目和恢复会话直接继承；显式 `--model provider/model` 只覆盖当前启动。`/provider key [provider-id]` 从安全输入通道更新已有 Key，支持模型发现的 Provider 必须用新 Key 验证成功才提交，失败自动恢复旧 Key。旧项目 `kernary.providers.toml` 仍可读取并在选中后迁移。协议页支持 OpenAI Responses、OpenAI Chat Completions 和 Anthropic Messages，并自动配置对应 endpoint、模型目录格式及 Bearer/x-api-key 鉴权。

向量模型使用 `/vector setup` 单独配置全局 Provider Catalog。Voyage AI 与 Jina AI 为前两个内置选项，只需输入 Key，模型 ID 已预填；Custom 要求厂商名、URL、Key，并支持一次录入多个模型 ID。所选模型必须返回合法数值向量，聊天模型等非向量模型不会保存。维度先从真实响应自动识别，兼容端点要求显式维度时才手动输入并再次验证。使用 `/vector providers` 查看目录，`/vector provider [id]` 先选厂商再选模型，`/vector model [id]` 切换当前厂商模型。Provider/Key 全局复用，每个项目启动时自动健康检查；Memory 和向量投影仍按项目隔离。`/vector clear` 二次确认后删除全部全局向量配置与凭证，并清除当前项目投影。

`/language en|zh-CN|zh-TW|ja` 可切换并持久化英语、简体中文、繁体中文和日语语言包。

产品界面把内部遥测与用户对话分层：主区只保留用户消息、Agent/Tool 活动、权限、错误与结果，详细状态仍可从 Event Log 查询。命令候选以悬浮面板显示，设置向导和 Secure Key 使用不同输入状态；顶部状态条会响应终端宽度显示 Git 分支、模型、Context 进度和运行中的 Agent。

普通对话使用后台执行通道，模型网络请求和工具循环不会再阻塞 TUI。Enter 后立即显示已发送/运行状态；流式 token 合并成连续段落，一个回答只显示一次 KERNARY 标签。界面实时显示有界处理摘要、Agent 状态、Tool、文件读取/修改、字节数、退出码和撤销能力，不展示私有 Chain-of-Thought。设置成功等短期消息改在右下角按结果分色显示 8 秒，下一条普通对话清除这些临时提示而保留真实对话与工具证据。

Provider、模型、向量厂商和 Session 等候选配置使用独立选择页：方向键移动，Enter 一次确认并立即进入下一步，Esc 返回聊天。向导中的 URL、验证过程和候选明细不会写入聊天历史，成功后只保留一条简洁结果。`clear`、`cls` 和 `/clear` 均为本地清屏命令，未配置模型时也不会触发 `MODEL_NOT_CONFIGURED`。

Shift+Tab 静默切换权限，不再向聊天区写入 `permissions.mode=...`。当前权限只在左下角以彩色徽标显示：MANUAL 黄、EDIT 青、AUTO 绿、FULL 紫、BYPASS 红。

方向键选择候选，Tab 补全，PgUp/PgDn 回看对话；API Key 通过独立 Secure Lane 输入并进入 OS Credential Store。终端编辑支持左右光标、Home/End、Delete/Backspace、Ctrl+A/E/U/K/W、Ctrl+Left/Right 和安全 Bracketed Paste。

`doctor`、`--help`、completion 和 man 生成不会创建项目 `.harness` 状态。

## Adaptive Agent Team

`/agents compact|verbose|tree` 可查看 30 个内置 Agent；它们默认 Sleeping。新增 Product、UX、Product Design、Design System、Frontend、Backend、API、Database/SQL、Quality、Accessibility、Platform/DevOps、SRE/Observability、Technical Writing、Localization 和Product Analytics专职角色。高保障任务可运行：

```text
/team adaptive 2 release secure auth service with performance benchmark
```

固定骨架为 Requirements + Explorer → Architect → Planner → Coder workers → Reviewer → Tester；目标命中安全、性能、发布类别时，分别增加 Security Auditor、Performance Engineer、Release Manager 证据门。所有 Agent 使用独立工作 Context、最小 Tool 视图和有界预算，Staffing Router 只读取结构化能力元数据。

30 个内置 Agent 均绑定版本化 AgentProfile v1：使命、非目标、必需输入、独立 SOP、输出/证据合同、失败升级、上下文、工具、记忆、模型预算和公开方法论来源各不相同。`/agent <id>` 可检查完整 Profile 与 Public Methodology。全栈目标构建24节点专职DAG；普通目标仍只唤醒必要角色。

轮次预算使用目标值、分段扩展和绝对上限，不把目标轮次当作质量截止线。预算段最后一轮停止新工具调用并要求完成或生成 PARTIAL_HANDOFF；部分结果与 continuation 先写入 agents.sqlite，仍有预算时自动续跑。连续三次相同工具调用会在第三次执行前被 Stuck Detector 拦截并换策略，避免无限循环。Balanced 默认全树并发为 3。

Cache Affinity ABI v2 把 Role Profile、项目稳定约束和固定 Tool Kit 放在 Prompt 前缀，把 Task/Run ID、依赖结果、检索结果与用户问题放在动态尾部。OpenAI Responses 使用稳定 `prompt_cache_key`，GPT-5.6+ 启用原生缓存选项；Anthropic Messages 同时缓存稳定 system/tool 前缀与增长中的对话。相同模型、Profile 和 Tool ABI 的并发 Agent 只在首个响应开始前短暂排队，随后立即并行，以减少同波重复 cache write。左下角常驻显示真实 Provider Prompt Cache 命中率；尚无请求时显示 `--`，之后按命中率分色，`/cache` 提供完整 read/write Token。

Grounded Hybrid Context Compaction v3 会在单 Agent、Team、Adaptive、Evidence 与 Review 开始前检查上下文。达到 80% 后先建 checkpoint，再保留 Goal、当前 Task、Pin、Constraint、Decision、Error、Exact 数据和完整 Tool 对，只有压缩后 Token 确实下降才 CAS 切换新 Series。已配置文本模型时必须返回带真实 Context 引用的结构化状态；引用无效、响应不完整、含 UserSecret 或模型不可用时自动改用本地原文抽取。已配置向量模型时，对可压缩旧记录进行有数量/Token 上限的语义重排，保留最贴合“项目目标 + 当前任务”的旧证据原文；未配置时完整走 lexical 路径。摘要进入当前项目可检索 Memory，完整 Transcript 始终保留。

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

自定义中转站优先通过 `/provider add` 写入用户全局目录。`examples/kernary.providers.toml` 仍可作为手工导入模板复制到旧项目根目录；选中其中的 Provider 后会迁入全局目录，示例只含 credential reference，不含真实 Key。

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

`kernary` 每次创建新 Session；`kernary -c` 继续当前项目最近会话，`kernary -r [id-or-title]` 从当前项目选择恢复。会话内使用 `/session`、`/session new`、`/session switch` 和 `/session rename`。第一条有效对话先生成本地临时标题，第一次完整回答后由当前模型在后台生成短主题；标题任务无工具、输出受限，失败保留临时名，且不会覆盖 `/session rename` 的手动名称。完整 Transcript 不随 Context 压缩删除。

`kernary -r` 和 `/session` 都使用 `↑/↓` 选择、Enter 直接恢复、Esc 返回的会话选择页，不再输入数字序号。用户界面显示 8 位短 ID（如 `#7ac91e2f`），短 ID 可直接传给 `-r` 或 `/session`；完整内部 ID 继续用于数据库关联，现有会话无需迁移。

项目私有指令位于 `.harness/agent.md`，不存在时才读取全局 `~/.kernary/agent.md`；使用 `/agentmd` 管理。全局向量 Provider/模型目录位于 Kernary 用户配置目录的 `vector.toml`，Key 按 Provider 分别保存在 OS Credential Store，Memory/Repository/Vector SQLite 则全部位于各项目 `.harness/`。旧项目向量配置会在全局配置缺失时自动迁移，旧版单 Provider 全局配置会升级为 `custom-legacy` 目录项；Kernary 继续写入 `.git/info/exclude`，项目辅助数据默认不会进入 Git。

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

未配置全局向量 Provider，也未设置 `KERNARY_EMBEDDING_MODEL`（legacy fallback：`HARNESS_EMBEDDING_MODEL`）时，Kernary 不构造 Embedding Provider/Vector Backend，不创建向量表、generation 或 job。项目启动健康检查通过后进入 Ready；首次 semantic/hybrid 请求才惰性激活当前项目投影，失败则明确降级为 lexical-only。

配置后，Vector 会自动沉淀高价值 Agent 合同、架构、决策、经验、验证与失败；代码库的 path/FTS/symbol/import/LSP 候选再经过语义重排，自然语言没有字面命中时使用结构中心文件作为 semantic seed。Query 和未变化文件的向量按 generation 与内容 hash 跨 Agent、Session 和重启复用，新建 generation 与候选文件使用批量 Embedding。主 Agent 和每波子 Agent 都接收带来源、状态、匹配通道与分数的检索数据，自动注入上限为 1800 Token。`/vector status` 展示覆盖、缓存、降级、注入、排名提升和知识写入收益。

## 命令与状态兼容

- `kernary` 与 `harness` 读取同一个 `.harness` 项目状态目录；
- 两者使用同一个 `dev.openai.harness` OS Credential Service；
- `KERNARY_*` 优先，`HARNESS_*` 作为兼容 fallback；
- Windows 一个周期内继续安装到旧默认目录 `Programs/Harness/bin`，避免现有 PATH 失效；
- Install/Rollback 会把 `kernary` 与 `harness` 当作一个原子 binary set。
