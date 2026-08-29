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

方向键选择候选，Tab 补全；API Key 通过独立 Secure Lane 输入并进入 OS Credential Store。终端编辑支持左右光标、Home/End、Delete/Backspace、Ctrl+A/E/U/K/W、Ctrl+Left/Right 和安全 Bracketed Paste。

`doctor`、`--help`、completion 和 man 生成不会创建项目 `.harness` 状态。

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

把 `examples/kernary.toml` 复制到项目根目录即可设置 Project 层。运行中可使用 `/config` 查看每项来源，使用 `/mode`、`/settings` 和 `/permissions` 写 Session 或 Runtime 层。`full` 也不能绕过 denied roots、Sandbox allowlist 或 Workspace Patch 强制二次审批。

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
