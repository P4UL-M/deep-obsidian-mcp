//! Exercise advertised API-key environment variables through the real CLI and
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
async fn real_cli_sends_environment_embedding_key_as_bearer() {
    for alias in [
        "DEEP_OBSIDIAN_EMBEDDING_API_KEY",
        "EMBEDDING_API_KEY",
        "OPENAI_API_KEY",
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
                .any(|line| line == "authorization: bearer fake-embedding-env-key");
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
        std::fs::write(
            &path,
            json!({
                "vaultPath": vault,
                "indexDir": directory.join("index"),
                "transport": "http",
                "http": {"host": "127.0.0.1", "port": port},
                "autoReindex": {"enabled": false},
                "embedding": {
                    "provider": "openai-compatible", "model": "test-embedding",
                    "baseUrl": format!("http://{embedding_address}/v1"),
                    "apiKeyRef": {"kind": "encryptedFile", "id": "intentionally-missing"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"));
        command
            .args(["--config", path.to_str().unwrap(), "serve"])
            .env_remove("DEEP_OBSIDIAN_EMBEDDING_API_KEY")
            .env_remove("EMBEDDING_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("DEEP_OBSIDIAN_AUTH_TOKEN")
            .env(alias, "fake-embedding-env-key")
            .env("XDG_CONFIG_HOME", directory.join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
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
                    "server exited for {alias}"
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
            "Bearer key missing for {alias}"
        );
        server.child.kill().await.unwrap();
        server.child.wait().await.unwrap();
    }
}
