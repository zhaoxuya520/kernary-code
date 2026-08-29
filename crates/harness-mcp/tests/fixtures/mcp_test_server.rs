use std::fs;
use std::io::{BufRead, Write};

fn main() {
    if let Some(marker) = std::env::args_os().nth(1) {
        fs::write(marker, b"started").expect("write marker");
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin line");
        let request: serde_json::Value = serde_json::from_str(&line).expect("request json");
        let Some(id) = request.get("id").and_then(serde_json::Value::as_u64) else {
            if request.get("method").and_then(serde_json::Value::as_str)
                == Some("notifications/initialized")
            {
                let notification = serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":"notifications/tools/list_changed"
                });
                writeln!(stdout, "{notification}").expect("write notification");
                stdout.flush().expect("flush notification");
            }
            continue;
        };
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion":"2025-11-25",
                "capabilities":{"tools":{},"resources":{},"prompts":{}},
                "serverInfo":{"name":"harness-rust-fixture","version":"1.0.0"},
                "instructions":"fixture only"
            }),
            "tools/list" => serde_json::json!({
                "tools":[
                    {
                        "name":"echo.read",
                        "description":"echo input text",
                        "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},
                        "annotations":{"readOnlyHint":true}
                    },
                    {
                        "name":"message.send",
                        "description":"send a message",
                        "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},
                        "annotations":{"destructiveHint":true}
                    },
                    {
                        "name":"long.required",
                        "description":"requires unsupported MCP task augmentation",
                        "inputSchema":{"type":"object"},
                        "execution":{"taskSupport":"required"}
                    }
                ]
            }),
            "resources/list" => serde_json::json!({
                "resources":[{"uri":"memory://guide","name":"Guide","mimeType":"text/plain","size":16}]
            }),
            "resources/read" => serde_json::json!({
                "contents":[{"uri":"memory://guide","mimeType":"text/plain","text":"fixture resource"}]
            }),
            "prompts/list" => serde_json::json!({
                "prompts":[{"name":"review","description":"review prompt","arguments":[]}]
            }),
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or_default();
                let arguments = request["params"]["arguments"].clone();
                serde_json::json!({
                    "content":[{"type":"text","text":arguments.get("text").and_then(serde_json::Value::as_str).unwrap_or_default()}],
                    "structuredContent":{"tool":name,"args":arguments}
                })
            }
            _ => {
                let response = serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "error":{"code":-32601,"message":"Method not found"}
                });
                writeln!(stdout, "{response}").expect("write error");
                stdout.flush().expect("flush error");
                continue;
            }
        };
        let response = serde_json::json!({"jsonrpc":"2.0","id":id,"result":result});
        writeln!(stdout, "{response}").expect("write response");
        stdout.flush().expect("flush response");
    }
}
