use std::fs;
use std::io::{BufRead, BufReader, Write};

fn main() {
    if let Some(marker) = std::env::args_os().nth(1) {
        fs::write(marker, b"started").expect("marker");
    }
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout().lock();
    let mut document_uri = String::new();
    while let Some(message) = read_message(&mut reader) {
        let method = message
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if method == "exit" {
            break;
        }
        if method == "textDocument/didOpen" {
            document_uri = message["params"]["textDocument"]["uri"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            write_message(
                &mut stdout,
                &serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":"textDocument/publishDiagnostics",
                    "params":{
                        "uri":document_uri,
                        "diagnostics":[{
                            "range":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}},
                            "severity":2,
                            "code":"fixture-warning",
                            "source":"kernary-fixture",
                            "message":"fixture diagnostic"
                        }]
                    }
                }),
            );
            continue;
        }
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match method {
            "initialize" => serde_json::json!({
                "capabilities":{
                    "textDocumentSync":1,
                    "positionEncoding":"utf-16",
                    "documentSymbolProvider":true,
                    "definitionProvider":true,
                    "referencesProvider":true,
                    "renameProvider":{"prepareProvider":true},
                    "codeActionProvider":{"resolveProvider":true}
                },
                "serverInfo":{"name":"kernary-lsp-fixture","version":"1.0"}
            }),
            "textDocument/documentSymbol" => serde_json::json!([{
                "name":"main",
                "detail":"fn main",
                "kind":12,
                "range":{"start":{"line":0,"character":0},"end":{"line":2,"character":1}},
                "selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}},
                "children":[{
                    "name":"value",
                    "kind":13,
                    "range":{"start":{"line":1,"character":4},"end":{"line":1,"character":9}},
                    "selectionRange":{"start":{"line":1,"character":4},"end":{"line":1,"character":9}}
                }]
            }]),
            "textDocument/definition" => {
                let position = message["params"]["position"].clone();
                serde_json::json!({
                    "uri":document_uri,
                    "range":{"start":position,"end":position}
                })
            }
            "textDocument/references" => {
                let position = message["params"]["position"].clone();
                serde_json::json!([{
                    "uri":document_uri,
                    "range":{"start":position,"end":position}
                },{
                    "uri":document_uri,
                    "range":{"start":{"line":2,"character":4},"end":{"line":2,"character":8}}
                }])
            }
            "textDocument/prepareRename" => serde_json::json!({
                "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},
                "placeholder":"fixture"
            }),
            "textDocument/rename" => workspace_edit(&document_uri, "renamed"),
            "textDocument/codeAction" => serde_json::json!([{
                "title":"Replace fixture token",
                "kind":"quickfix",
                "edit":workspace_edit(&document_uri, "fixed")
            },{
                "title":"Resolved fixture action",
                "kind":"refactor.rewrite",
                "data":{"fixture":true}
            }]),
            "codeAction/resolve" => {
                let mut action = message["params"].clone();
                action["edit"] = workspace_edit(&document_uri, "resolved");
                action
            }
            "shutdown" => serde_json::Value::Null,
            _ => {
                write_message(
                    &mut stdout,
                    &serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "error":{"code":-32601,"message":"Method not found"}
                    }),
                );
                continue;
            }
        };
        write_message(
            &mut stdout,
            &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
        );
    }
}

fn workspace_edit(uri: &str, new_text: &str) -> serde_json::Value {
    let mut changes = serde_json::Map::new();
    changes.insert(
        uri.to_owned(),
        serde_json::json!([{
            "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},
            "newText":new_text
        }]),
    );
    serde_json::json!({"changes":changes})
}

fn read_message<R: BufRead>(reader: &mut R) -> Option<serde_json::Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0_u8; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message<W: Write>(writer: &mut W, message: &serde_json::Value) {
    let body = serde_json::to_vec(message).expect("json");
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).expect("header");
    writer.write_all(&body).expect("body");
    writer.flush().expect("flush");
}
