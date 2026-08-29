# Security Policy

## Supported versions

当前仅维护最新发布版本。

## Reporting a vulnerability

请使用 GitHub 仓库的 Private vulnerability reporting 功能提交安全问题，不要在公开 Issue 中包含漏洞细节、API Key、Token、项目源码或可直接利用的 PoC。

报告建议包含：受影响版本、平台、最小复现、影响范围和建议修复。收到报告后会先确认问题，再协调修复与披露时间。

## Security boundaries

- Permission policy 不能替代 Sandbox；
- Sandbox hard deny 不能被 Allow Rule 或 Full Mode 覆盖；
- API Key/OAuth token 不进入日志、Context、npm 包或仓库；
- 未配置 Embedding Model 时不会初始化 Vector capability；
- Workspace Patch 必须经过独立预览与二次审批。
