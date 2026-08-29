# Security Policy

## Supported versions

当前仅维护最新发布版本。

## Reporting a vulnerability

请使用 GitHub 仓库的 Private vulnerability reporting 功能提交安全问题，不要在公开 Issue 中包含漏洞细节、API Key、Token、项目源码或可直接利用的 PoC。

报告建议包含：受影响版本、平台、最小复现、影响范围和建议修复。收到报告后会先确认问题，再协调修复与披露时间。

## Security boundaries

- Permission policy 不能替代 Sandbox；
- Sandbox hard deny 不能被 Allow Rule 或 Full Mode 覆盖；
- 默认 `workspace-write`；Windows 以受限 Token + capability ACL 强制写边界，Linux 以 bubblewrap namespace 强制文件与网络边界；
- `.git` 与 `.harness` 在 `workspace-write` 中仍不可由子进程写入；`read-only` 只允许隔离临时目录写入；
- Linux 缺少可信 `bwrap` 或平台后端启动失败时受限命令 fail closed，不静默退化；
- Windows unelevated 后端的文件写边界是内核强制，默认断网是环境兼容层而非 WFP；`/sandbox` 会如实显示此差异；
- Windows 项目 capability ACE 幂等写入；账户缺少 `WRITE_DAC` 时命令 fail closed；
- `danger-full-access`、Sandbox 内联网与权限 `bypass` 都需要独立显式确认；项目配置不能静默启用它们；
- API Key/OAuth token 不进入日志、Context、npm 包或仓库；
- 未配置 Embedding Model 时不会初始化 Vector capability；
- Embedding Provider/模型目录与各 Provider 凭证是全局的，但 Memory、Repository 与向量投影数据库仍按项目保存在 `.harness/`；
- 新增或切换 Embedding 模型时必须返回单个非空、有限数值向量；普通聊天模型不会被接受为向量模型；
- 每次项目启动只用固定健康检查文本验证全局 Embedding Provider，不上传项目源码；真正的语义检索才会发送被选中的项目记忆/查询文本；
- Workspace Patch 必须经过独立预览与二次审批。
