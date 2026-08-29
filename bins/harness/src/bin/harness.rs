//! `harness` 兼容命令；所有逻辑与 `kernary` 共用同一入口和状态。

#[path = "../main.rs"]
mod kernary_main;

fn main() -> std::process::ExitCode {
    kernary_main::main()
}
