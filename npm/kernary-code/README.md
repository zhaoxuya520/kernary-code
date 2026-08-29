# Kernary Code

Terminal-native multi-agent coding runtime.

```bash
npm install -g kernary-code
kernary --help
kernary exec --json "review this project"
```

On first launch, use `/connect` and `/model`. Kernary refuses normal and non-interactive work with `MODEL_NOT_CONFIGURED` until a real or local model is ready; the published default never runs the deterministic test provider.

The interactive editor supports Unicode cursor movement, Home/End, Delete/Backspace, common Ctrl line-editing shortcuts, bracketed paste, and a scrollable `/` command palette with Provider/Model completion.

The package installs a native binary selected by npm for Windows x64 or Linux x64 glibc. It does not download executables during `postinstall`.

Source, documentation and checksums: https://github.com/zhaoxuya520/kernary-code
