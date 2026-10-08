//! Exercise stored credentials and the explicit environment override through the real CLI and
//! startup indexing HTTP request, with no process-wide environment mutation.
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server {
    child: tokio::process::Child,
    directory: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn real_cli_preserves_secret_references_overrides_and_mcp_auth() {
    for (label, has_reference, stored_key, override_key, expected_key) in [
        (
            "override-only",
            false,
            None,
            Some("explicit-key"),
            "explicit-key",
        ),
        (
            "missing-reference-override",
            true,
            None,
            Some("explicit-key"),
            "explicit-key",
        ),
        (
            "stored-reference",
            true,
            Some("stored-key"),
            None,
            "stored-key",
        ),
        (
            "stored-reference-override",
            true,
            Some("stored-key"),
            Some("explicit-key"),
            "explicit-key",
        ),
        (
            "blank-override",
            true,
            Some("stored-key"),
            Some(" \t"),
            "stored-key",
        ),
    ] {
        let embedding_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let embedding_address = embedding_listener.local_addr().unwrap();
        let mock = tokio::spawn(async move {
            let (mut stream, _) = embedding_listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request closed before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
            let headers = headers.to_ascii_lowercase();
            assert!(headers.starts_with("post /v1/embeddings http/1.1\r\n"));
            let authenticated = headers
                .lines()
                .any(|line| line == format!("authorization: bearer {expected_key}"));
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            while request.len() < header_end + length {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request closed before body");
                request.extend_from_slice(&buffer[..read]);
            }
            let body: Value = serde_json::from_slice(&request[header_end..]).unwrap();
            let inputs = body["input"].as_array().unwrap();
            assert!(!inputs.is_empty());
            let response = json!({"data": inputs.iter().enumerate().map(|(index, _)| {
                json!({"index": index, "embedding": [1.0, 0.0, 0.0]})
            }).collect::<Vec<_>>()})
            .to_string();
            let status = if authenticated {
                "200 OK"
            } else {
                "401 Unauthorized"
            };
            stream
                .write_all(format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                ).as_bytes())
                .await
                .unwrap();
            authenticated
        });

        let directory = std::env::temp_dir().join(format!(
            "deep-obsidian-embedding-auth-{}",
            deep_obsidian_server::auth::generate_token()
        ));
        let vault = directory.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("Note.md"), "# Note\nA note to embed.").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let path = directory.join("config.json");
        let mut config = json!({
            "vaultPath": vault,
            "indexDir": directory.join("index"),
            "transport": "http",
            "http": {"host": "127.0.0.1", "port": port},
            "autoReindex": {"enabled": false},
            "auth": {
                "enabled": true,
                "oauth": {"issuerUrl": "https://mcp.example.test"}
            },
            "embedding": {
                "provider": "openai-compatible", "model": "test-embedding",
                "baseUrl": format!("http://{embedding_address}/v1")
            }
        });
        if has_reference {
            config["embedding"]["apiKeyRef"] =
                json!({"kind":"encryptedFile","id":"embedding-reference"});
        }
        let config_text = config.to_string();
        std::fs::write(&path, &config_text).unwrap();
        if let Some(key) = stored_key {
            let mut store = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"));
            let mut child = store
                .args([
                    "--config",
                    path.to_str().unwrap(),
                    "secrets",
                    "set",
                    "--target",
                    "embedding-api-key",
                    "--stdin",
                ])
                .env("XDG_CONFIG_HOME", directory.join("config"))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("{key}\n").as_bytes())
                .await
                .unwrap();
            assert!(
                child.wait().await.unwrap().success(),
                "store reference for {label}"
            );
        }
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"));
        command
            .args(["--config", path.to_str().unwrap(), "serve"])
            .env_remove("DEEP_OBSIDIAN_EMBEDDING_API_KEY")
            .env("DEEP_OBSIDIAN_AUTH_TOKEN", "existing-mcp-owner-token")
            .env("XDG_CONFIG_HOME", directory.join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(key) = override_key {
            command.env("DEEP_OBSIDIAN_EMBEDDING_API_KEY", key);
        }
        let mut server = Server {
            child: command.spawn().unwrap(),
            directory,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "server exited for {label}"
                );
                if client
                    .get(format!("http://127.0.0.1:{port}/readyz"))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("embedding-backed index becomes ready");
        assert!(
            tokio::time::timeout(Duration::from_secs(10), mock)
                .await
                .expect("embedding request reaches mock")
                .unwrap(),
            "Bearer key missing for {label}"
        );
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"embedding-auth-test","version":"1"}
        }});
        let owner = client
            .post(&endpoint)
            .bearer_auth("existing-mcp-owner-token")
            .header("accept", "application/json, text/event-stream")
            .json(&initialize)
            .send()
            .await
            .unwrap();
        assert!(
            owner.status().is_success(),
            "existing MCP token for {label}"
        );
        let embedding = client
            .post(&endpoint)
            .bearer_auth(expected_key)
            .header("accept", "application/json, text/event-stream")
            .json(&initialize)
            .send()
            .await
            .unwrap();
        assert_eq!(
            embedding.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "embedding key is not an MCP token"
        );
        let oauth: Value = client
            .get(format!(
                "http://127.0.0.1:{port}/.well-known/oauth-authorization-server"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(oauth["issuer"], "https://mcp.example.test");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            config_text,
            "existing config is preserved"
        );
        server.child.kill().await.unwrap();
        server.child.wait().await.unwrap();
    }
}
