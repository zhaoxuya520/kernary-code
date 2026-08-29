use std::fs;
use std::io::{Read, Write};

fn main() {
    let operation = std::env::args().nth(1).unwrap_or_default();
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read request");
    let request: serde_json::Value = serde_json::from_str(&input).expect("request json");
    let response = match operation.as_str() {
        "activate" => {
            if let Some(marker) = request["settings"]["marker"].as_str() {
                fs::write(marker, b"activated").expect("write marker");
            }
            if request["settings"]["crashActivate"].as_bool() == Some(true) {
                std::process::exit(42);
            } else if request["settings"]["failActivate"].as_bool() == Some(true) {
                serde_json::json!({"ok":false})
            } else {
                serde_json::json!({"ok":true})
            }
        }
        "deactivate" => serde_json::json!({"ok":true}),
        "tool-call" => {
            let tool = request["tool"].as_str().unwrap_or_default();
            let text = request["arguments"]["text"].as_str().unwrap_or_default();
            let result = match tool {
                "uppercase" => serde_json::Value::String(text.to_uppercase()),
                _ => serde_json::json!({"unknown":tool}),
            };
            serde_json::json!({"result":result})
        }
        _ => serde_json::json!({"ok":false}),
    };
    std::io::stdout()
        .write_all(response.to_string().as_bytes())
        .expect("write response");
}
