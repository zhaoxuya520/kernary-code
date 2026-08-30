# Kernary Evaluation

Kernary 使用两张互不冒充的成绩单。

## 1. Local Product Score

这是确定性工程门禁，覆盖构建质量、运行时正确性、安全边界、MCP、恢复、Context/Memory、Provider/TUI 和跨实现契约。它证明产品没有已知工程退化，但不能单独证明 Agent 智能超过其他 CLI。

```bash
python scripts/evaluate-cli.py --profile quick
python scripts/evaluate-cli.py --profile full
```

报告默认写入 `output/evals/local-scorecard.json`。`output/` 不进入公开提交。

## 2. External Agent Score

跨 CLI 比较必须满足：

1. 同一模型与 Provider；
2. 同一 reasoning、上下文、费用和墙钟预算；
3. 同一数据集固定版本和容器镜像；
4. Kernary、Codex CLI、Claude Code 各至少 5 个随机种子；
5. 同时报告成功率、成本、延迟、Token、工具错误和恢复次数；
6. 使用 bootstrap 95% 置信区间；只有 Kernary 下界高于对手上界才称为“超过”；
7. 公开原始轨迹、失败分类、版本哈希和完整运行命令；
8. 未运行一律标记 `not-run`，不得用本地单测代替。

## 必须通过的行业基准

| 基准 | 证明内容 | Kernary 验收条件 |
|---|---|---|
| MCP Conformance stable suites | MCP 协议与 OAuth/transport 行为 | 0 unexpected failures |
| Terminal-Bench / Harbor | 通用终端任务完成能力 | 同模型配对分数显著高于 Codex 与 Claude Code |
| Claw-SWE-Bench Lite | Harness、成本与真实 issue 修复 | resolve rate 更高且单次成功成本不更差 |
| SWE-bench Multilingual | 9 种语言的仓库级工程能力 | 每种语言不低于基线，macro average 更高 |
| Aider Polyglot | 225 个多语言代码编辑任务 | pass@2 更高，well-formed edit=100% |
| BFCL V4 Agentic | Tool 选择、参数、并行、多轮与恢复 | 所有报告类别的同模型 harness delta 为正 |
| τ-bench | Tool+用户+政策约束下的可靠性 | pass^1 更高且无 policy regression |

SWE-bench Verified 只保留兼容性参考，不作为唯一领先证据；其污染和区分度问题已被公开指出。优先使用持续更新的 Terminal-Bench、SWE-bench Multilingual 和低成本 Claw-SWE-Bench Lite。

## 官方入口

- [MCP Conformance](https://github.com/modelcontextprotocol/conformance)
- [Terminal-Bench / Harbor](https://github.com/harbor-framework/terminal-bench)
- [Harbor custom agents](https://www.harborframework.com/docs/hosted-harbor/custom-agents)
- [SWE-bench Multilingual](https://www.swebench.com/multilingual.html)
- [Claw-SWE-Bench](https://github.com/opensquilla/claw-swe-bench)
- [Aider Polyglot benchmark](https://github.com/Aider-AI/aider/tree/main/benchmark)
- [BFCL](https://github.com/EnlightenedAI/BFCL)
- [τ-bench](https://github.com/sierra-research/tau-bench)
