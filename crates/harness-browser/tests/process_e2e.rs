use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use harness_browser::*;
use harness_types::{BrowserActionId, BrowserSessionId, ConfidentialityLabel};
use tempfile::tempdir;

#[test]
#[ignore = "设置 HARNESS_BROWSER_E2E=1 并提供本机 Playwright/Chrome 后运行"]
fn playwright_process_adapter_runs_structured_loopback_browser_session() {
    if std::env::var("HARNESS_BROWSER_E2E").ok().as_deref() != Some("1") {
        return;
    }
    let python = PathBuf::from(std::env::var_os("HARNESS_BROWSER_PYTHON").expect("python path"));
    let browser =
        PathBuf::from(std::env::var_os("HARNESS_BROWSER_EXECUTABLE").expect("browser path"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    let running = Arc::new(AtomicBool::new(true));
    let server_running = running.clone();
    let server = thread::spawn(move || {
        let html = br#"<!doctype html><html><head><title>Rust Browser E2E</title></head><body>
          <h1 id="status">idle</h1>
          <label>Message <input aria-label="Message" id="message"></label>
          <label>Upload <input aria-label="Upload" id="upload" type="file"></label>
          <label>Password <input aria-label="Password" type="password"></label>
          <label>Code <input aria-label="Code" autocomplete="one-time-code"></label>
          <a aria-label="Download" download="report.txt" href="data:text/plain,download-evidence">Download</a>
          <button aria-label="Apply" onclick="document.querySelector('#status').textContent=document.querySelector('#message').value">Apply</button>
        </body></html>"#;
        while server_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        html.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(html);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    let temporary = tempdir().expect("tempdir");
    let profile = temporary.path().join("profile");
    let artifacts = temporary.path().join("artifacts");
    let journal = Arc::new(
        SqliteBrowserJournal::open(temporary.path().join("browser.sqlite")).expect("journal"),
    );
    let adapter = Arc::new(PlaywrightProcessAdapter::new(python).expect("adapter"));
    let runtime = BrowserRuntime::new(
        BrowserSessionConfig {
            id: BrowserSessionId::from("browser:e2e"),
            browser_executable: browser,
            profile_directory: profile,
            artifact_directory: artifacts.clone(),
            download_directory: temporary.path().join("downloads"),
            headless: true,
            allowed_origins: [origin.clone()].into_iter().collect(),
            upload_roots: vec![temporary.path().to_path_buf()],
            allow_uploads: true,
            allow_downloads: true,
            timeout_millis: 30_000,
        },
        adapter,
        journal.clone(),
    )
    .expect("runtime");
    runtime.open(1).expect("open");
    runtime
        .execute(
            BrowserActionId::from("action:navigate"),
            BrowserCommand::Navigate {
                url: format!("{origin}/"),
            },
            2,
        )
        .expect("navigate");
    let first = runtime
        .execute(
            BrowserActionId::from("action:snapshot-1"),
            BrowserCommand::Snapshot,
            3,
        )
        .expect("snapshot");
    let BrowserResult::Snapshot { snapshot } = first else {
        panic!("snapshot result")
    };
    assert_eq!(snapshot.title, "Rust Browser E2E");
    let textbox = snapshot
        .nodes
        .iter()
        .find(|node| node.role == "textbox" && node.name == "Message")
        .and_then(|node| node.ref_id.clone())
        .expect("textbox ref");
    let button = snapshot
        .nodes
        .iter()
        .find(|node| node.role == "button" && node.name == "Apply")
        .and_then(|node| node.ref_id.clone())
        .expect("button ref");
    let upload = snapshot
        .nodes
        .iter()
        .find(|node| node.name == "Upload")
        .and_then(|node| node.ref_id.clone())
        .expect("upload ref");
    let download = snapshot
        .nodes
        .iter()
        .find(|node| node.role == "link" && node.name == "Download")
        .and_then(|node| node.ref_id.clone())
        .expect("download ref");
    for name in ["Password", "Code"] {
        let sensitive = snapshot
            .nodes
            .iter()
            .find(|node| node.name == name)
            .expect("sensitive node");
        assert!(sensitive.sensitive);
        assert!(sensitive.ref_id.is_none());
    }
    runtime
        .execute(
            BrowserActionId::from("action:type"),
            BrowserCommand::Type {
                ref_id: textbox.clone(),
                text: "Harness Browser".to_owned(),
                classification: ConfidentialityLabel::ProjectPrivate,
            },
            4,
        )
        .expect("type");
    let read = runtime
        .execute(
            BrowserActionId::from("action:read"),
            BrowserCommand::Read { ref_id: textbox },
            5,
        )
        .expect("read");
    assert_eq!(
        read,
        BrowserResult::Text {
            text: "Harness Browser".to_owned()
        }
    );
    assert!(matches!(
        runtime
            .execute(
                BrowserActionId::from("action:inspect"),
                BrowserCommand::Inspect {
                    ref_id: button.clone()
                },
                6
            )
            .expect("inspect"),
        BrowserResult::Inspect { .. }
    ));
    let upload_path = temporary.path().join("upload.txt");
    std::fs::write(&upload_path, "upload evidence").expect("upload fixture");
    runtime
        .execute(
            BrowserActionId::from("action:upload"),
            BrowserCommand::Upload {
                ref_id: upload,
                path: upload_path,
            },
            7,
        )
        .expect("upload");
    let downloaded = runtime
        .execute(
            BrowserActionId::from("action:download"),
            BrowserCommand::Download { ref_id: download },
            8,
        )
        .expect("download");
    let BrowserResult::Artifact {
        artifact: downloaded,
    } = downloaded
    else {
        panic!("download artifact")
    };
    assert!(
        downloaded
            .path
            .starts_with(temporary.path().join("downloads"))
    );
    assert!(downloaded.bytes > 0);
    runtime
        .execute(
            BrowserActionId::from("action:click"),
            BrowserCommand::Click { ref_id: button },
            9,
        )
        .expect("click");
    runtime
        .execute(
            BrowserActionId::from("action:wait"),
            BrowserCommand::Wait {
                wait: BrowserWait::Millis { millis: 50 },
            },
            10,
        )
        .expect("wait");
    let after = runtime
        .execute(
            BrowserActionId::from("action:snapshot-2"),
            BrowserCommand::Snapshot,
            11,
        )
        .expect("snapshot after");
    assert!(matches!(
        after,
        BrowserResult::Snapshot { snapshot }
            if snapshot.nodes.iter().any(|node| node.role == "heading" && node.name == "Harness Browser")
    ));
    let shot = runtime
        .execute(
            BrowserActionId::from("action:screenshot"),
            BrowserCommand::Screenshot,
            12,
        )
        .expect("screenshot");
    let BrowserResult::Artifact { artifact } = shot else {
        panic!("artifact")
    };
    assert!(artifact.path.starts_with(&artifacts));
    assert!(artifact.bytes > 1_000);
    assert_eq!(
        runtime
            .execute(
                BrowserActionId::from("action:deny"),
                BrowserCommand::Navigate {
                    url: "https://example.com/".to_owned()
                },
                13
            )
            .expect_err("origin deny")
            .code,
        "browser-origin-not-allowed"
    );
    runtime.close(14).expect("close");
    let actions = journal
        .list(&BrowserSessionId::from("browser:e2e"))
        .expect("actions");
    assert_eq!(actions.len(), 12);
    assert_eq!(
        actions.last().expect("last").status,
        BrowserActionStatus::Failed
    );
    running.store(false, Ordering::SeqCst);
    server.join().expect("server");
}
