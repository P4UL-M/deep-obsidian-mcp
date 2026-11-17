//! Exercise the real CLI, HTTP bootstrap and MCP/upload handlers with temporary
//! vaults and fake credentials. No process-wide env or user's keychain is touched.
use reqwest::{header, StatusCode, Url};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};

const SECRET: &str = "fake-http-integration-owner-secret";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "https://client.example/callback";

struct Server {
    child: tokio::process::Child,
    directory: PathBuf,
    base: String,
    client: reqwest::Client,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Server {
    async fn start(oauth: bool) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "deep-obsidian-http-oauth-{}",
            deep_obsidian_server::auth::generate_token()
        ));
        let vault = directory.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        drop(listener);
        let auth = if oauth {
            json!({"oauth": {"issuerUrl": base, "accessTokenTtlSeconds": 3600}})
        } else {
            json!({})
        };
        let config = json!({"vaultPath": vault, "indexDir": directory.join("index"), "transport":"http", "http":{"host":"127.0.0.1", "port":port}, "autoReindex":{"enabled":false}, "auth":auth});
        let path = directory.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-mcp"))
            .args(["--config", path.to_str().unwrap(), "serve"])
            .env("DEEP_OBSIDIAN_AUTH_TOKEN", SECRET)
            .env("XDG_CONFIG_HOME", directory.join("config"))
            .env_remove("DEEP_OBSIDIAN_ALLOW_INSECURE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut server = Self {
            child,
            directory,
            base,
            client,
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "HTTP server exited before readiness"
                );
                if server
                    .client
                    .get(format!("{}/healthz", server.base))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("server starts within 10s");
        server
    }

    async fn mcp(&self, bearer: Option<&str>, method: &str, params: Value) -> reqwest::Response {
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        request.send().await.unwrap()
    }
}

#[tokio::test]
async fn cli_probe_reports_http_authentication_errors_and_accepts_the_service_token() {
    let server = Server::start(false).await;
    for authenticated in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"));
        command
            .arg("--config")
            .arg(server.directory.join("config.json"))
            .args(["--json", "probe"])
            .env("XDG_CONFIG_HOME", server.directory.join("probe-client"))
            .env_remove("DEEP_OBSIDIAN_AUTH_TOKEN");
        if authenticated {
            command.env("DEEP_OBSIDIAN_AUTH_TOKEN", SECRET);
        }
        let output = command.output().await.unwrap();
        assert_eq!(output.status.success(), authenticated);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["health"]["ok"], true);
        assert_eq!(report["mcp"]["ok"], authenticated);
        if !authenticated {
            let error = report["mcp"]["error"].as_str().unwrap();
            assert!(error.contains("HTTP 401 Unauthorized"), "{error}");
            assert!(error.contains("bearer token"), "{error}");
            assert!(!error.contains("decoding response body"), "{error}");
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
    }
}

#[tokio::test]
async fn real_http_legacy_bearer_and_mcp_api_remain_unchanged() {
    let server = Server::start(false).await;
    let response = server.mcp(None, "initialize", json!({})).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(
        server
            .client
            .get(format!(
                "{}/.well-known/oauth-authorization-server",
                server.base
            ))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = server.mcp(Some(SECRET), "initialize", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.unwrap();
    assert!(value["result"]["serverInfo"]["name"].is_string());
    assert_eq!(
        server
            .client
            .put(format!("{}/upload/unknown", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn real_http_oauth_discovery_consent_mcp_and_binary_upload() {
    let server = Server::start(true).await;
    let response = server.mcp(None, "initialize", json!({})).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers()[header::WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .contains("resource_metadata="));
    let metadata: Value = server
        .client
        .get(format!(
            "{}/.well-known/oauth-protected-resource",
            server.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metadata["resource"], format!("{}/mcp", server.base));
    let response: Value = server
        .client
        .post(format!("{}/register", server.base))
        .json(&json!({"redirect_uris":[REDIRECT], "token_endpoint_auth_method":"none"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = response["client_id"].as_str().unwrap();
    let response = server
        .client
        .get(format!("{}/authorize", server.base))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
            ("state", "test-state"),
            ("resource", format!("{}/mcp", server.base).as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = response.text().await.unwrap();
    let nonce = page
        .split("name=\"request_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let response = server
        .client
        .post(format!("{}/authorize", server.base))
        .header(header::ORIGIN, &server.base)
        .header(header::COOKIE, cookie)
        .form(&[
            ("request_id", nonce),
            ("password", SECRET),
            ("decision", "allow"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let redirect = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let code = redirect
        .query_pairs()
        .find(|(name, _)| name == "code")
        .unwrap()
        .1
        .into_owned();
    let response = server
        .client
        .post(format!("{}/token", server.base))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT),
            ("code", code.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let token: Value = response.json().await.unwrap();
    let access = token["access_token"].as_str().unwrap();
    let response = server.mcp(Some(access), "initialize", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.json::<Value>().await.unwrap()["result"]["serverInfo"]["name"].is_string());
    for bearer in [SECRET, access] {
        let response = server.mcp(Some(bearer),"tools/call",json!({"name":"request_vault_upload", "arguments":{"path":"Attachments/test.bin","maxBytes":1024}})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        assert!(
            value["result"].get("isError").is_none() || value["result"]["isError"] == false,
            "{value}"
        );
        let body: Value =
            serde_json::from_str(value["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let url = body["uploadUrl"].as_str().unwrap();
        assert_eq!(
            server
                .client
                .put(url)
                .body("binary-test")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let uploaded = server
            .client
            .put(url)
            .bearer_auth(bearer)
            .body("binary-test")
            .send()
            .await
            .unwrap();
        assert_eq!(uploaded.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(server.directory.join("vault/Attachments/test.bin")).unwrap(),
            b"binary-test"
        );
    }
    assert!((2591990..=2592000).contains(&token["refresh_token_expires_in"].as_u64().unwrap()));
    let refresh = token["refresh_token"].as_str().unwrap();
    let response = server
        .client
        .post(format!("{}/token", server.base))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let renewed: Value = response.json().await.unwrap();
    assert_ne!(renewed["refresh_token"], token["refresh_token"]);
    assert_eq!(
        server
            .mcp(
                Some(renewed["access_token"].as_str().unwrap()),
                "initialize",
                json!({})
            )
            .await
            .status(),
        StatusCode::OK
    );
    let replay = server
        .client
        .post(format!("{}/token", server.base))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        server
            .mcp(Some(access), "initialize", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .mcp(
                Some(renewed["access_token"].as_str().unwrap()),
                "initialize",
                json!({})
            )
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .mcp(Some(SECRET), "initialize", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
}

async fn cli_json(server: &Server, args: &[&str]) -> Value {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_deep-obsidian-cli"))
        .args([
            "--config",
            server.directory.join("config.json").to_str().unwrap(),
            "--json",
        ])
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn user_tokens(server: &Server, name: &str, password: &str) -> (String, Value) {
    let registered: Value = server
        .client
        .post(format!("{}/register", server.base))
        .json(&json!({"redirect_uris":[REDIRECT]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client = registered["client_id"].as_str().unwrap().to_owned();
    let response = server
        .client
        .get(format!("{}/authorize", server.base))
        .query(&[
            ("response_type", "code"),
            ("client_id", client.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_challenge", CHALLENGE),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let page = response.text().await.unwrap();
    assert!(page.contains("name=\"username\""));
    let nonce = page
        .split("name=\"request_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let response = server
        .client
        .post(format!("{}/authorize", server.base))
        .header(header::ORIGIN, &server.base)
        .header(header::COOKIE, cookie)
        .form(&[
            ("request_id", nonce),
            ("decision", "allow"),
            ("username", name),
            ("password", password),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    let code = location
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .to_string();
    let tokens: Value = server
        .client
        .post(format!("{}/token", server.base))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client.as_str()),
            ("redirect_uri", REDIRECT),
            ("code", code.as_str()),
            ("code_verifier", VERIFIER),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tokens["access_token"].is_string(), "{tokens}");
    (client, tokens)
}

#[tokio::test]
async fn multiuser_cli_activation_oauth_rotation_revocation_and_classic_transition() {
    let server = Server::start(true).await;
    std::fs::write(
        server.directory.join("vault/note.md"),
        "# Shared note\nhello",
    )
    .unwrap();
    let plan = server.directory.join("plan.json");
    std::fs::write(&plan,json!([
        {"name":"owner","grants":[{"mount_id":"vault","role":"owner"}],"default_mount":"vault"},
        {"name":"reader","grants":[{"mount_id":"vault","role":"reader"}],"default_mount":"vault"}
    ]).to_string()).unwrap();
    let created = cli_json(
        &server,
        &["users", "enable", "--plan", plan.to_str().unwrap(), "--yes"],
    )
    .await;
    let password = created["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["user"]["name"] == "reader")
        .unwrap()["password"]
        .as_str()
        .unwrap();
    assert_eq!(
        server
            .mcp(Some(SECRET), "initialize", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let health: Value = server
        .client
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(health.get("mounts").is_none() && health.get("vaultPath").is_none());
    let (client, tokens) = user_tokens(&server, "reader", password).await;
    let access = tokens["access_token"].as_str().unwrap();
    let response: Value = server
        .mcp(
            Some(access),
            "tools/call",
            json!({"name":"read_file","arguments":{"path":"note.md"}}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(response["result"]["structuredContent"]["text"]
        .as_str()
        .unwrap()
        .contains("hello"));
    let response: Value = server
        .mcp(
            Some(access),
            "tools/call",
            json!({"name":"upsert_note","arguments":{"path":"denied.md","content":"blocked"}}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_some());
    cli_json(&server, &["users", "revoke", "reader", "--mount", "vault"]).await;
    let response: Value = server
        .mcp(
            Some(access),
            "tools/call",
            json!({"name":"read_file","arguments":{"path":"note.md"}}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_some());
    cli_json(
        &server,
        &[
            "users", "grant", "reader", "--mount", "vault", "--role", "reader",
        ],
    )
    .await;
    // Upload tokens remain bound to their issuing user, even when another
    // authorized user can write the same shared mount. Consumption checks the
    // current role again after the token was minted.
    let owner_password = created["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["user"]["name"] == "owner")
        .unwrap()["password"]
        .as_str()
        .unwrap();
    let (_, owner_tokens) = user_tokens(&server, "owner", owner_password).await;
    let owner_access = owner_tokens["access_token"].as_str().unwrap();
    cli_json(
        &server,
        &[
            "users", "grant", "reader", "--mount", "vault", "--role", "editor",
        ],
    )
    .await;
    let mint = |path: &str| json!({"name":"request_vault_upload","arguments":{"path":path,"maxBytes":1024}});
    let minted: Value = server
        .mcp(
            Some(owner_access),
            "tools/call",
            mint("Attachments/owned.bin"),
        )
        .await
        .json()
        .await
        .unwrap();
    let url = minted["result"]["structuredContent"]["uploadUrl"]
        .as_str()
        .unwrap();
    let wrong_user = server
        .client
        .put(url)
        .bearer_auth(access)
        .body("wrong user")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_user.status(), StatusCode::FORBIDDEN);
    let uploaded = server
        .client
        .put(url)
        .bearer_auth(owner_access)
        .body("owner bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read(server.directory.join("vault/Attachments/owned.bin")).unwrap(),
        b"owner bytes"
    );
    let minted: Value = server
        .mcp(
            Some(owner_access),
            "tools/call",
            mint("Attachments/revoked.bin"),
        )
        .await
        .json()
        .await
        .unwrap();
    let url = minted["result"]["structuredContent"]["uploadUrl"]
        .as_str()
        .unwrap();
    cli_json(
        &server,
        &[
            "users", "grant", "reader", "--mount", "vault", "--role", "owner",
        ],
    )
    .await;
    cli_json(
        &server,
        &[
            "users", "grant", "owner", "--mount", "vault", "--role", "reader",
        ],
    )
    .await;
    let revoked = server
        .client
        .put(url)
        .bearer_auth(owner_access)
        .body("revoked bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::FORBIDDEN);
    assert!(!server
        .directory
        .join("vault/Attachments/revoked.bin")
        .exists());
    let rotated = cli_json(&server, &["users", "rotate-password", "reader"]).await;
    assert_ne!(rotated["password"], password);
    assert_eq!(
        server
            .mcp(Some(access), "initialize", json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = server
        .client
        .post(format!("{}/token", server.base))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client.as_str()),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (_, new_tokens) =
        user_tokens(&server, "reader", rotated["password"].as_str().unwrap()).await;
    cli_json(&server, &["users", "disable", "--yes"]).await;
    assert_eq!(
        server
            .mcp(
                Some(new_tokens["access_token"].as_str().unwrap()),
                "initialize",
                json!({})
            )
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .mcp(Some(SECRET), "initialize", json!({}))
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        std::fs::read_to_string(server.directory.join("vault/note.md"))
            .unwrap()
            .contains("hello")
    );
}
