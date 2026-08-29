# Kernary Code

Terminal-native multi-agent coding runtime.

```bash
npm install -g kernary-code
kernary --help
kernary exec --json "review this project"
```

On first launch, use `/connect` and `/model`. Kernary refuses normal and non-interactive work with `MODEL_NOT_CONFIGURED` until a real or local model is ready; the published default never runs the deterministic test provider.

The transcript-first TUI keeps internal telemetry out of the conversation, presents project/model/context/agent state in a responsive semantic header, and uses a floating `/` command palette plus distinct normal, setup, and secure-key composer states. PgUp/PgDn scroll the transcript without disturbing input history. The editor also supports Unicode cursor movement, Home/End, Delete/Backspace, common Ctrl line-editing shortcuts, and bracketed paste.

Use `/provider add` for a custom OpenAI-compatible endpoint (URL → secure key → model discovery → default selection), `/provider switch` to switch providers, and `/model` to switch within the current provider. `/vector setup` configures one independently validated embedding endpoint with a user-entered model name. `/language en|zh-CN|zh-TW|ja` switches the customized language pack.

Kernary includes 15 sleeping-by-default agents. `/team adaptive <1..4> <objective>` builds a capability-routed evidence DAG with Requirements, Explorer, Architect, Planner, Coder workers, Reviewer, and Tester; Security, Performance, and Release gates are added only when the objective requires them. Each specialist receives an isolated context, a minimal tool view, and a role-specific evidence contract.

The package installs a native binary selected by npm for Windows x64 or Linux x64 glibc. It does not download executables during `postinstall`.

Source, documentation and checksums: https://github.com/zhaoxuya520/kernary-code
