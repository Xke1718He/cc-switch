#![cfg(feature = "headless")]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn invoke(client: &Client, base: &str, command: &str, args: Value) -> Value {
    let response = client
        .post(format!("{base}/api/invoke/{command}"))
        .json(&json!({ "args": args }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{command}: {status}: {body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn headless_matches_current_provider_mode_and_session_contracts() {
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join(".claude/projects/web-test");
    std::fs::create_dir_all(&project).unwrap();
    let session = project.join("session-test.jsonl");
    let line = json!({
        "sessionId": "session-test", "type": "user", "uuid": "message-test",
        "timestamp": "2026-11-02T00:00:00Z",
        "message": { "role": "user", "content": "你好 Web" },
        "imageData": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j3XkAAAAASUVORK5CYII="
    })
    .to_string();
    std::fs::write(&session, format!("{line}\n")).unwrap();
    let log = tempfile::tempfile().unwrap();
    let port = unused_port();
    let base = format!("http://127.0.0.1:{port}");
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_cc-switchd"))
            .env("CC_SWITCH_TEST_HOME", home.path())
            .env("HOME", home.path())
            .env("CC_SWITCH_WEB_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..150 {
        if client
            .get(format!("{base}/api/health"))
            .send()
            .await
            .is_ok()
        {
            ready = true;
            break;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "daemon did not start");
    for app in ["mcode", "kimi", "dsh", "agy", "omp"] {
        let dir = invoke(&client, &base, "get_config_dir", json!({ "app": app })).await;
        assert!(dir.is_string(), "{app}: {dir}");
    }
    let provider = json!({
        "id": "web-test", "name": "Web test",
        "settingsConfig": { "env": { "ANTHROPIC_BASE_URL": "https://example.invalid", "ANTHROPIC_API_KEY": "test-key" } }
    });
    assert_eq!(
        invoke(
            &client,
            &base,
            "add_provider",
            json!({ "app": "claude", "provider": provider, "addToLive": false })
        )
        .await,
        true
    );
    let rows = invoke(&client, &base, "get_providers", json!({ "app": "claude" })).await;
    assert_eq!(rows["web-test"]["name"], "Web test");
    let editor = invoke(
        &client,
        &base,
        "get_provider_editor_view",
        json!({
            "app": "claude", "providerId": "web-test", "settingsConfig": provider["settingsConfig"]
        }),
    )
    .await;
    assert_eq!(editor["settings"]["env"]["ANTHROPIC_API_KEY"], "test-key");
    let mut updated = provider.clone();
    updated["name"] = json!("Web updated");
    assert_eq!(
        invoke(
            &client,
            &base,
            "update_provider",
            json!({
                "app": "claude", "provider": updated, "editorSave": { "base": editor["settings"] }
            })
        )
        .await,
        true
    );
    assert_eq!(
        invoke(
            &client,
            &base,
            "get_app_mode",
            json!({ "appType": "claude" })
        )
        .await["mode"],
        "direct"
    );
    let mut config = invoke(&client, &base, "get_global_proxy_config", json!({})).await;
    config["listenPort"] = json!(unused_port());
    config["listenAddress"] = json!("127.0.0.1");
    invoke(
        &client,
        &base,
        "update_global_proxy_config",
        json!({ "config": config }),
    )
    .await;
    invoke(
        &client,
        &base,
        "set_proxy_takeover_for_app",
        json!({ "appType": "claude", "enabled": true, "route": "web-test", "stack": false }),
    )
    .await;
    assert_eq!(
        invoke(
            &client,
            &base,
            "get_app_mode",
            json!({ "appType": "claude" })
        )
        .await["mode"],
        "route"
    );
    invoke(
        &client,
        &base,
        "set_auto_failover_enabled",
        json!({ "appType": "claude", "enabled": true }),
    )
    .await;
    let mut app_config = invoke(
        &client,
        &base,
        "get_proxy_config_for_app",
        json!({ "appType": "claude" }),
    )
    .await;
    assert_eq!(app_config["autoFailoverEnabled"], true);
    // Saving retry/timeouts cannot silently change the route mode's enabled mirror.
    app_config["enabled"] = json!(false);
    invoke(
        &client,
        &base,
        "update_proxy_config_for_app",
        json!({ "config": app_config }),
    )
    .await;
    assert_eq!(
        invoke(
            &client,
            &base,
            "get_proxy_config_for_app",
            json!({ "appType": "claude" })
        )
        .await["enabled"],
        true
    );
    invoke(&client, &base, "stop_proxy_with_restore", json!({})).await;
    assert_eq!(
        invoke(
            &client,
            &base,
            "get_app_mode",
            json!({ "appType": "claude" })
        )
        .await["mode"],
        "direct"
    );

    let args = json!({ "providerId": "claude", "sourcePath": session });
    let response = client
        .post(format!("{base}/api/invoke/stream_session_messages"))
        .json(&json!({ "args": args }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.headers()["content-type"], "application/x-ndjson");
    let body = response.text().await.unwrap();
    let chunks: Vec<Value> = body
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(chunks.first().unwrap()["type"], "header");
    assert_eq!(chunks.first().unwrap()["total"], 1);
    assert_eq!(chunks[1]["messages"][0]["blocks"][0]["text"], "你好 Web");
    assert_eq!(chunks.last().unwrap()["type"], "done");
    let content = invoke(&client, &base, "get_session_block_content", json!({
        "providerId": "claude", "sourcePath": session,
        "contentRef": { "kind": "jsonl", "offset": 0, "len": line.len(), "pointer": "/message/content" },
        "offset": 0, "limit": 2
    })).await;
    assert_eq!(content["text"], "你好");
    assert_eq!(content["truncated"], true);
    let response = client
        .post(format!("{base}/api/invoke/get_session_image"))
        .json(&json!({ "args": {
            "providerId": "claude", "sourcePath": session,
            "image": { "source": { "kind": "inline", "content": {
                "kind": "jsonl", "offset": 0, "len": line.len(), "pointer": "/imageData"
            } }, "mediaType": "image/png", "size": 68 }
        } }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    let expected = base64::engine::general_purpose::STANDARD
        .decode(
            serde_json::from_str::<Value>(&line).unwrap()["imageData"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        expected.as_slice()
    );

    // A source outside the tool's session roots must still be rejected over HTTP.
    let response = client.post(format!("{base}/api/invoke/stream_session_messages"))
        .json(&json!({ "args": { "providerId": "claude", "sourcePath": home.path().join(".cc-switch/settings.json") } }))
        .send().await.unwrap();
    let error: Value = serde_json::from_str(response.text().await.unwrap().trim()).unwrap();
    assert_eq!(error["type"], "error");
}
