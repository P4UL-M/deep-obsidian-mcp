// Included inside oauth::tests to reuse its HTTP fixture and PKCE flow.
async fn renewable_grant(f: &Fixture, client: &str) -> serde_json::Value {
    let code = issue_code(f, client).await;
    let response = exchange(f, client, &code, VERIFIER, None, REDIRECT).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn renew(
    f: &Fixture,
    client: &str,
    credential: &str,
    resource: Option<&str>,
    scope: Option<&str>,
) -> reqwest::Response {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client),
        ("refresh_token", credential),
    ];
    if let Some(resource) = resource {
        form.push(("resource", resource));
    }
    if let Some(scope) = scope {
        form.push(("scope", scope));
    }
    f.client
        .post(format!("{}/token", f.base))
        .form(&form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn refresh_rotates_credentials_without_extending_thirty_day_authorization() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let old_refresh = initial["refresh_token"].as_str().unwrap();
    let old_access = initial["access_token"].as_str().unwrap();
    assert!((2591990..=2592000).contains(&initial["refresh_token_expires_in"].as_u64().unwrap()));
    assert!(!f.oauth.accepts(old_refresh));
    let (family_key, expires) = {
        let mut store = f.oauth.store.lock().unwrap();
        store.tokens.get_mut(&hash(old_access)).unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        let key = store.refresh_tokens[&hash(old_refresh)];
        (key, store.families[&key].expires)
    };
    assert!(!f.oauth.accepts(old_access));
    let response = renew(
        &f,
        &client,
        old_refresh,
        Some(&format!("{}/mcp", f.base)),
        Some("obsidian"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let rotated: serde_json::Value = response.json().await.unwrap();
    assert_eq!(rotated["expires_in"], 3600);
    assert_ne!(rotated["refresh_token"], initial["refresh_token"]);
    assert_ne!(rotated["access_token"], initial["access_token"]);
    assert!(f.oauth.accepts(rotated["access_token"].as_str().unwrap()));
    assert_eq!(
        f.oauth.store.lock().unwrap().families[&family_key].expires,
        expires
    );
    assert!(rotated["refresh_token_expires_in"].as_u64().unwrap() <= 2592000);
    assert_eq!(
        renew(
            &f,
            &client,
            rotated["refresh_token"].as_str().unwrap(),
            None,
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn replay_revokes_all_tokens_of_that_authorization_only() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let unrelated = renewable_grant(&f, &client).await;
    let old_refresh = initial["refresh_token"].as_str().unwrap();
    let rotated: serde_json::Value = renew(&f, &client, old_refresh, None, None)
        .await
        .json()
        .await
        .unwrap();
    assert!(f.oauth.accepts(initial["access_token"].as_str().unwrap()));
    assert!(f.oauth.accepts(rotated["access_token"].as_str().unwrap()));
    assert_eq!(
        renew(&f, &client, old_refresh, None, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!f.oauth.accepts(initial["access_token"].as_str().unwrap()));
    assert!(!f.oauth.accepts(rotated["access_token"].as_str().unwrap()));
    assert_eq!(
        renew(
            &f,
            &client,
            rotated["refresh_token"].as_str().unwrap(),
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(f.oauth.accepts(unrelated["access_token"].as_str().unwrap()));
    assert_eq!(
        renew(
            &f,
            &client,
            unrelated["refresh_token"].as_str().unwrap(),
            None,
            None
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        f.client
            .post(format!("{}/mcp", f.base))
            .bearer_auth(SECRET)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_is_bound_to_client_scope_and_resource() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let other = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let credential = initial["refresh_token"].as_str().unwrap();
    for (id, resource, scope, expected) in [
        (other.as_str(), None, None, "invalid_grant"),
        (
            client.as_str(),
            Some("https://other.example/mcp"),
            None,
            "invalid_target",
        ),
        (client.as_str(), None, Some("admin"), "invalid_scope"),
    ] {
        let response = renew(&f, id, credential, resource, scope).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            expected
        );
    }
    assert_eq!(
        renew(
            &f,
            &client,
            initial["access_token"].as_str().unwrap(),
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        renew(&f, &client, "unknown-token", None, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        renew(&f, &client, credential, None, None).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn refresh_family_expiration_and_client_removal_revoke_access() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let credential = initial["refresh_token"].as_str().unwrap();
    {
        let mut store = f.oauth.store.lock().unwrap();
        let family = store.refresh_tokens[&hash(credential)];
        store.families.get_mut(&family).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        renew(&f, &client, credential, None, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!f.oauth.accepts(initial["access_token"].as_str().unwrap()));
    assert!(f.oauth.store.lock().unwrap().refresh_tokens.is_empty());
    let initial = renewable_grant(&f, &client).await;
    f.oauth.store.lock().unwrap().clients.remove(&client);
    assert!(!f.oauth.accepts(initial["access_token"].as_str().unwrap()));
    assert_eq!(
        renew(
            &f,
            &client,
            initial["refresh_token"].as_str().unwrap(),
            None,
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn concurrent_refresh_replay_revokes_the_family() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let credential = initial["refresh_token"].as_str().unwrap();
    let (first, second) = tokio::join!(
        renew(&f, &client, credential, None, None),
        renew(&f, &client, credential, None, None)
    );
    let mut statuses = vec![first.status().as_u16(), second.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, vec![200, 400]);
    assert!(f.oauth.store.lock().unwrap().families.is_empty());
    assert!(f.oauth.store.lock().unwrap().tokens.is_empty());
}

#[tokio::test]
async fn refresh_can_be_disabled_globally_or_for_a_registered_client() {
    let f = fixture_with_refresh(0).await;
    let metadata: serde_json::Value = f
        .client
        .get(format!("{}/.well-known/oauth-authorization-server", f.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        metadata["grant_types_supported"],
        json!(["authorization_code"])
    );
    let client = register_client(&f).await;
    assert!(renewable_grant(&f, &client)
        .await
        .get("refresh_token")
        .is_none());
    let f = fixture().await;
    let response: serde_json::Value = f
        .client
        .post(format!("{}/register", f.base))
        .json(&json!({"redirect_uris":[REDIRECT], "grant_types":["authorization_code"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client = response["client_id"].as_str().unwrap();
    assert!(renewable_grant(&f, client)
        .await
        .get("refresh_token")
        .is_none());
    let metadata: serde_json::Value = f
        .client
        .get(format!("{}/.well-known/oauth-authorization-server", f.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        metadata["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    let legacy: Client = serde_json::from_value(json!({"redirect_uris":[REDIRECT]})).unwrap();
    assert!(legacy.allow_refresh);
}

#[tokio::test]
async fn refresh_history_capacity_does_not_consume_a_valid_credential() {
    let f = fixture().await;
    let client = register_client(&f).await;
    let initial = renewable_grant(&f, &client).await;
    let credential = initial["refresh_token"].as_str().unwrap();
    {
        let mut store = f.oauth.store.lock().unwrap();
        let family = store.refresh_tokens[&hash(credential)];
        for i in 0..REFRESH_CAPACITY - 1 {
            store
                .refresh_tokens
                .insert(hash(&format!("spent-{i}")), family);
        }
    }
    assert_eq!(
        renew(&f, &client, credential, None, None).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    f.oauth
        .store
        .lock()
        .unwrap()
        .refresh_tokens
        .retain(|key, _| *key == hash(credential));
    assert_eq!(
        renew(&f, &client, credential, None, None).await.status(),
        StatusCode::OK
    );
}
