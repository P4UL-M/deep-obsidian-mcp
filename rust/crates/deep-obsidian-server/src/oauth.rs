//! Small single-owner OAuth authority. Public clients register exact redirects,
//! then use Authorization Code + PKCE S256. The owner's legacy bearer is only
//! entered into our consent form; it is never issued to an OAuth client.
use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use deep_obsidian_types::OAuthConfig;
use reqwest::Url;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{auth::generate_token, mcp::AppState};

const CAPACITY: usize = 1024;
const REFRESH_CAPACITY: usize = 8192;
const CODE_TTL: Duration = Duration::from_secs(300);
const COOKIE: &str = "deep_obsidian_consent";

#[derive(Clone, Serialize, Deserialize)]
struct Client {
    redirect_uris: Vec<String>,
    // Older registries had only redirect_uris. Keep those clients renewable.
    #[serde(default = "refresh_allowed_by_default")]
    allow_refresh: bool,
}

fn refresh_allowed_by_default() -> bool {
    true
}

struct AccessToken {
    expires: Instant,
    family: Option<[u8; 32]>,
}

struct RefreshFamily {
    client_id: String,
    expires: Instant,
    current: [u8; 32],
}

#[derive(Clone, Deserialize)]
struct AuthorizationRequest {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

struct Pending {
    request: AuthorizationRequest,
    cookie_hash: [u8; 32],
    expires: Instant,
}

struct Code {
    request: AuthorizationRequest,
    expires: Instant,
}

struct Store {
    clients: HashMap<String, Client>,
    pending: HashMap<[u8; 32], Pending>,
    codes: HashMap<[u8; 32], Code>,
    tokens: HashMap<[u8; 32], AccessToken>,
    families: HashMap<[u8; 32], RefreshFamily>,
    // Retain spent hashes until family expiry to detect replay after rotation.
    refresh_tokens: HashMap<[u8; 32], [u8; 32]>,
    failed_logins: u32,
    failure_window: Instant,
}

pub struct OAuthState {
    issuer: String,
    resource: String,
    ttl: Duration,
    refresh_ttl: Duration,
    client_file: Option<PathBuf>,
    store: Mutex<Store>,
}

fn hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

impl Store {
    fn prune(&mut self) {
        let now = Instant::now();
        self.pending.retain(|_, pending| pending.expires > now);
        self.codes.retain(|_, code| code.expires > now);
        self.families.retain(|_, family| {
            family.expires > now && self.clients.contains_key(&family.client_id)
        });
        self.refresh_tokens
            .retain(|_, family| self.families.contains_key(family));
        self.tokens.retain(|_, token| {
            token.expires > now
                && token
                    .family
                    .is_none_or(|family| self.families.contains_key(&family))
        });
        if now.duration_since(self.failure_window) >= Duration::from_secs(60) {
            self.failed_logins = 0;
            self.failure_window = now;
        }
    }

    fn revoke_family(&mut self, family: [u8; 32]) {
        self.families.remove(&family);
        self.refresh_tokens.retain(|_, owner| *owner != family);
        self.tokens.retain(|_, token| token.family != Some(family));
    }
}

impl OAuthState {
    /// Config is validated by the config loader. Registration survives restarts;
    /// grants and bearer tokens deliberately do not.
    pub fn new(
        config: &OAuthConfig,
        mcp_path: &str,
        client_file: Option<PathBuf>,
    ) -> io::Result<Self> {
        let clients = match client_file.as_ref().map(fs::read).transpose() {
            Ok(Some(bytes)) => {
                serde_json::from_slice::<HashMap<String, Client>>(&bytes).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid OAuth client registry")
                })?
            }
            Ok(None) => HashMap::new(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error),
        };
        if clients.len() > CAPACITY
            || clients.values().any(|client| {
                client.redirect_uris.is_empty()
                    || client.redirect_uris.len() > 16
                    || client.redirect_uris.iter().any(|uri| !valid_redirect(uri))
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid OAuth client registry",
            ));
        }
        Ok(Self {
            issuer: config.issuer_url.clone(),
            resource: format!("{}{mcp_path}", config.issuer_url),
            ttl: Duration::from_secs(config.access_token_ttl_seconds),
            refresh_ttl: Duration::from_secs(config.refresh_token_ttl_seconds),
            client_file,
            store: Mutex::new(Store {
                clients,
                pending: HashMap::new(),
                codes: HashMap::new(),
                tokens: HashMap::new(),
                families: HashMap::new(),
                refresh_tokens: HashMap::new(),
                failed_logins: 0,
                failure_window: Instant::now(),
            }),
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn accepts(&self, token: &str) -> bool {
        let mut store = self.store.lock().expect("OAuth store lock");
        store.prune();
        store.tokens.contains_key(&hash(token))
    }

    fn save_clients(&self, clients: &HashMap<String, Client>) -> io::Result<()> {
        let Some(path) = &self.client_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("{}.tmp", generate_token()));
        let result = (|| {
            use std::io::Write;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec(clients)?)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

fn error(status: StatusCode, name: &'static str) -> Response {
    secure((status, Json(json!({"error": name}))).into_response())
}

fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    response
}

pub fn routes(mcp_path: &str) -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            &format!("/.well-known/oauth-protected-resource{mcp_path}"),
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/register", post(register))
        .route("/authorize", get(authorize).post(consent))
        .route("/token", post(token))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}

async fn resource_metadata(State(state): State<AppState>) -> Response {
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    secure(Json(json!({"resource": oauth.resource, "authorization_servers": [oauth.issuer], "scopes_supported": ["obsidian"], "bearer_methods_supported": ["header"]})).into_response())
}

async fn server_metadata(State(state): State<AppState>) -> Response {
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    secure(Json(json!({
        "issuer": oauth.issuer,
        "authorization_endpoint": format!("{}/authorize", oauth.issuer),
        "token_endpoint": format!("{}/token", oauth.issuer),
        "registration_endpoint": format!("{}/register", oauth.issuer),
        "response_types_supported": ["code"], "grant_types_supported": if oauth.refresh_ttl.is_zero() { vec!["authorization_code"] } else { vec!["authorization_code", "refresh_token"] },
        "token_endpoint_auth_methods_supported": ["none"], "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["obsidian"]
    })).into_response())
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

fn valid_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    uri.len() <= 2048
        && url.host_str().is_some()
        && (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && !url
            .query_pairs()
            .any(|(key, _)| matches!(key.as_ref(), "code" | "state" | "iss" | "error"))
}

async fn register(
    State(state): State<AppState>,
    request: Result<Json<Registration>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    };
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if request.redirect_uris.is_empty()
        || request.redirect_uris.len() > 16
        || request.redirect_uris.iter().any(|uri| !valid_redirect(uri))
    {
        return error(StatusCode::BAD_REQUEST, "invalid_redirect_uri");
    }
    let grants = request.grant_types.clone().unwrap_or_else(|| {
        if oauth.refresh_ttl.is_zero() {
            vec!["authorization_code".into()]
        } else {
            vec!["authorization_code".into(), "refresh_token".into()]
        }
    });
    if request
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
        || !grants.iter().any(|grant| grant == "authorization_code")
        || grants.len() > 2
        || grants
            .iter()
            .any(|grant| grant != "authorization_code" && grant != "refresh_token")
        || (grants.len() == 2 && grants[0] == grants[1])
        || (oauth.refresh_ttl.is_zero() && grants.iter().any(|grant| grant == "refresh_token"))
        || request
            .response_types
            .as_ref()
            .is_some_and(|types| types != &["code"])
    {
        return error(StatusCode::BAD_REQUEST, "invalid_client_metadata");
    }
    let mut store = oauth.store.lock().expect("OAuth store lock");
    if store.clients.len() >= CAPACITY {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let client_id = generate_token();
    store.clients.insert(
        client_id.clone(),
        Client {
            redirect_uris: request.redirect_uris.clone(),
            allow_refresh: grants.iter().any(|grant| grant == "refresh_token"),
        },
    );
    if oauth.save_clients(&store.clients).is_err() {
        store.clients.remove(&client_id);
        return error(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
    }
    secure(
        (
            StatusCode::CREATED,
            Json(
                json!({"client_id": client_id, "redirect_uris": request.redirect_uris,
        "token_endpoint_auth_method": "none", "grant_types": grants, "response_types": ["code"]}),
            ),
        )
            .into_response(),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == challenge)
}

fn authorization_error(
    oauth: &OAuthState,
    request: &AuthorizationRequest,
    name: &'static str,
) -> Response {
    let mut redirect = Url::parse(&request.redirect_uri).expect("validated registered redirect");
    {
        let mut query = redirect.query_pairs_mut();
        query.append_pair("error", name);
        if let Some(state) = &request.state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", &oauth.issuer);
    }
    secure(Redirect::to(redirect.as_str()).into_response())
}

async fn authorize(
    State(state): State<AppState>,
    Query(request): Query<AuthorizationRequest>,
) -> Response {
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut store = oauth.store.lock().expect("OAuth store lock");
    store.prune();
    if !store
        .clients
        .get(&request.client_id)
        .is_some_and(|client| client.redirect_uris.contains(&request.redirect_uri))
    {
        // Never redirect unvalidated requests, even to report an error.
        return error(StatusCode::BAD_REQUEST, "invalid_client");
    }
    if request
        .state
        .as_ref()
        .is_some_and(|state| state.len() > 2048)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    if request.response_type.is_empty()
        || request.code_challenge_method != "S256"
        || !valid_challenge(&request.code_challenge)
    {
        return authorization_error(oauth, &request, "invalid_request");
    }
    if request.response_type != "code" {
        return authorization_error(oauth, &request, "unsupported_response_type");
    }
    if request
        .resource
        .as_ref()
        .is_some_and(|resource| resource != &oauth.resource)
    {
        return authorization_error(oauth, &request, "invalid_target");
    }
    if request
        .scope
        .as_deref()
        .is_some_and(|scope| scope != "obsidian")
    {
        return authorization_error(oauth, &request, "invalid_scope");
    }
    if store.pending.len() >= CAPACITY {
        return authorization_error(oauth, &request, "temporarily_unavailable");
    }
    let nonce = generate_token();
    let cookie = generate_token();
    let page = format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Deep Obsidian authorization</title><h1>Allow access to Deep Obsidian?</h1><p>This client will be able to read and modify your vault.</p><p>Client: <code>{}</code></p><p>Return URL: <code>{}</code></p><form method=\"post\" action=\"/authorize\"><input type=\"hidden\" name=\"request_id\" value=\"{}\"><label>Server secret <input type=\"password\" name=\"password\" required autocomplete=\"current-password\"></label><button name=\"decision\" value=\"allow\">Allow access</button><button name=\"decision\" value=\"deny\" formnovalidate>Cancel</button></form></html>", escape(&request.client_id), escape(&request.redirect_uri), nonce);
    store.pending.insert(
        hash(&nonce),
        Pending {
            request,
            cookie_hash: hash(&cookie),
            expires: Instant::now() + CODE_TTL,
        },
    );
    let mut response = secure(Html(page).into_response());
    let secure_cookie = if oauth.issuer.starts_with("https:") {
        "; Secure"
    } else {
        ""
    };
    response.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!("{COOKIE}={cookie}; Path=/authorize; Max-Age=300; HttpOnly; SameSite=Lax{secure_cookie}")).expect("generated cookie"));
    response
}

#[derive(Deserialize)]
struct Consent {
    request_id: String,
    password: Option<String>,
    decision: String,
}

async fn consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<Consent>,
) -> Response {
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        != Some(oauth.issuer.as_str())
    {
        return error(StatusCode::FORBIDDEN, "invalid_request");
    }
    // Zeroize the submitted owner credential on every return path.
    let password = form.password.map(secrecy::SecretString::new);
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .filter_map(|cookie| cookie.trim().split_once('='))
                .find(|(name, _)| *name == COOKIE)
                .map(|(_, value)| value)
        });
    let mut store = oauth.store.lock().expect("OAuth store lock");
    store.prune();
    let key = hash(&form.request_id);
    let Some(pending) = store.pending.get(&key) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if !cookie.is_some_and(|cookie| bool::from(hash(cookie).ct_eq(&pending.cookie_hash))) {
        return error(StatusCode::FORBIDDEN, "invalid_request");
    }
    if store.failed_logins >= 30 {
        return error(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable");
    }
    if form.decision != "deny"
        && (form.decision != "allow"
            || !state
                .auth
                .token
                .as_ref()
                .zip(password.as_ref())
                .is_some_and(|(secret, password)| {
                    bool::from(
                        secret
                            .expose_secret()
                            .as_bytes()
                            .ct_eq(password.expose_secret().as_bytes()),
                    )
                }))
    {
        store.failed_logins += 1;
        store.pending.remove(&key);
        return error(StatusCode::UNAUTHORIZED, "access_denied");
    }
    if store.codes.len() >= CAPACITY {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let pending = store.pending.remove(&key).expect("validated consent");
    let mut redirect = Url::parse(&pending.request.redirect_uri).expect("registered redirect");
    {
        let mut query = redirect.query_pairs_mut();
        if form.decision == "deny" {
            query.append_pair("error", "access_denied");
        } else {
            let code = generate_token();
            query.append_pair("code", &code);
            store.codes.insert(
                hash(&code),
                Code {
                    request: pending.request.clone(),
                    expires: Instant::now() + CODE_TTL,
                },
            );
        }
        if let Some(state) = &pending.request.state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", &oauth.issuer);
    }
    let mut response = secure(Redirect::to(redirect.as_str()).into_response());
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "deep_obsidian_consent=; Path=/authorize; Max-Age=0; HttpOnly; SameSite=Lax",
        ),
    );
    response
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_verifier: String,
    refresh_token: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

async fn token(
    State(state): State<AppState>,
    request: Result<Form<TokenRequest>, axum::extract::rejection::FormRejection>,
) -> Response {
    let Ok(Form(request)) = request else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Some(oauth) = &state.auth.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !matches!(
        request.grant_type.as_str(),
        "authorization_code" | "refresh_token"
    ) {
        return error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let mut store = oauth.store.lock().expect("OAuth store lock");
    store.prune();
    if request.grant_type == "refresh_token" {
        return refresh(oauth, &mut store, request);
    }
    if request.code.is_empty()
        || request.client_id.is_empty()
        || request.redirect_uri.is_empty()
        || request.code_verifier.is_empty()
    {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    // Consume under one lock even on a failed exchange: no replay or verifier guessing.
    let Some(code) = store.codes.remove(&hash(&request.code)) else {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(request.code_verifier.as_bytes()));
    if code.request.client_id != request.client_id
        || code.request.redirect_uri != request.redirect_uri
        || !valid_verifier(&request.code_verifier)
        || !bool::from(
            challenge
                .as_bytes()
                .ct_eq(code.request.code_challenge.as_bytes()),
        )
    {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    if request
        .resource
        .as_ref()
        .is_some_and(|resource| resource != &oauth.resource)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_target");
    }
    if store.tokens.len() >= CAPACITY {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let allow_refresh = !oauth.refresh_ttl.is_zero()
        && store
            .clients
            .get(&request.client_id)
            .is_some_and(|client| client.allow_refresh);
    let family = if allow_refresh {
        if store.families.len() >= CAPACITY || store.refresh_tokens.len() >= REFRESH_CAPACITY {
            return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
        }
        let family = hash(&generate_token());
        store.families.insert(
            family,
            RefreshFamily {
                client_id: request.client_id,
                expires: Instant::now() + oauth.refresh_ttl,
                current: [0; 32],
            },
        );
        Some(family)
    } else {
        None
    };
    issue_tokens(oauth, &mut store, family)
}

fn refresh(oauth: &OAuthState, store: &mut Store, request: TokenRequest) -> Response {
    let Some(presented) = request.refresh_token.filter(|token| !token.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    };
    if request.client_id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let token_hash = hash(&presented);
    let Some(family_key) = store.refresh_tokens.get(&token_hash).copied() else {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let family = store
        .families
        .get(&family_key)
        .expect("pruned refresh family");
    if family.client_id != request.client_id || oauth.refresh_ttl.is_zero() {
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    // A spent credential reveals replay. Revoke current refresh and all family access tokens.
    if family.current != token_hash {
        store.revoke_family(family_key);
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    if request
        .resource
        .as_ref()
        .is_some_and(|resource| resource != &oauth.resource)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_target");
    }
    if request
        .scope
        .as_deref()
        .is_some_and(|scope| scope != "obsidian")
    {
        return error(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    if store.tokens.len() >= CAPACITY || store.refresh_tokens.len() >= REFRESH_CAPACITY {
        return error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    issue_tokens(oauth, store, Some(family_key))
}

fn issue_tokens(oauth: &OAuthState, store: &mut Store, family_key: Option<[u8; 32]>) -> Response {
    let now = Instant::now();
    let lifetime = family_key.map_or(oauth.ttl, |key| {
        oauth
            .ttl
            .min(store.families[&key].expires.saturating_duration_since(now))
    });
    if lifetime.is_zero() {
        if let Some(key) = family_key {
            store.revoke_family(key);
        }
        return error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let access = generate_token();
    store.tokens.insert(
        hash(&access),
        AccessToken {
            expires: now + lifetime,
            family: family_key,
        },
    );
    let mut body = json!({"access_token": access, "token_type": "Bearer", "expires_in": lifetime.as_secs(), "scope": "obsidian", "resource": oauth.resource});
    if let Some(key) = family_key {
        let refresh = generate_token();
        let refresh_hash = hash(&refresh);
        let family = store.families.get_mut(&key).expect("refresh family");
        family.current = refresh_hash;
        body["refresh_token"] = json!(refresh);
        body["refresh_token_expires_in"] =
            json!(family.expires.saturating_duration_since(now).as_secs());
        store.refresh_tokens.insert(refresh_hash, key);
    }
    secure(Json(body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::AuthState, mounts::MountBackends, runtime::MountRuntimes};
    use deep_obsidian_types::ServiceConfigInput;
    use std::sync::Arc;

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const SECRET: &str = "test-owner-secret-not-a-real-token";
    const REDIRECT: &str = "https://client.example/callback?existing=yes";

    struct Fixture {
        base: String,
        client: reqwest::Client,
        oauth: Arc<OAuthState>,
        handle: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn fixture() -> Fixture {
        fixture_with_refresh(deep_obsidian_types::default_oauth_refresh_ttl()).await
    }

    async fn fixture_with_refresh(refresh_ttl: u64) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let config = deep_obsidian_config::normalize_service_config(ServiceConfigInput {
            vault_path: Some(std::env::temp_dir()),
            ..Default::default()
        })
        .unwrap();
        let backends = MountBackends::build(&config);
        let runtimes = MountRuntimes::new(&config, &backends);
        let oauth = Arc::new(
            OAuthState::new(
                &OAuthConfig {
                    issuer_url: base.clone(),
                    access_token_ttl_seconds: 3600,
                    refresh_token_ttl_seconds: refresh_ttl,
                },
                "/mcp",
                None,
            )
            .unwrap(),
        );
        let state = AppState::with_backends(config, runtimes, &backends).with_auth(AuthState {
            enabled: true,
            token: Some(secrecy::SecretString::new(SECRET.to_string())),
            oauth: Some(oauth.clone()),
            allowed_origins: Arc::new(vec!["https://allowed.example".into()]),
        });
        let protected = Router::new()
            .route("/mcp", post(|| async { "MCP allowed" }))
            .route(
                "/upload/{token}",
                axum::routing::put(|| async { "Upload allowed" }),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::auth::require_auth,
            ));
        let app = routes("/mcp").merge(protected).with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fixture {
            base,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            oauth,
            handle,
        }
    }

    async fn register_client(f: &Fixture) -> String {
        let response = f
            .client
            .post(format!("{}/register", f.base))
            .json(&json!({"redirect_uris": [REDIRECT], "token_endpoint_auth_method": "none"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let value: serde_json::Value = response.json().await.unwrap();
        assert!(value.get("client_secret").is_none());
        value["client_id"].as_str().unwrap().to_string()
    }

    fn params<'a>(client: &'a str, challenge: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", REDIRECT),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "some state&with=specials"),
            ("scope", "obsidian"),
        ]
    }

    async fn begin(f: &Fixture, client: &str) -> (String, String) {
        let response = f
            .client
            .get(format!("{}/authorize", f.base))
            .query(&params(client, CHALLENGE))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let page = response.text().await.unwrap();
        assert!(!page.contains(SECRET));
        let nonce = page
            .split("name=\"request_id\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_string();
        (nonce, cookie)
    }

    async fn consent_response(f: &Fixture, client: &str, password: &str) -> reqwest::Response {
        let (nonce, cookie) = begin(f, client).await;
        f.client
            .post(format!("{}/authorize", f.base))
            .header(header::ORIGIN, &f.base)
            .header(header::COOKIE, cookie)
            .form(&[
                ("request_id", nonce.as_str()),
                ("password", password),
                ("decision", "allow"),
            ])
            .send()
            .await
            .unwrap()
    }

    async fn issue_code(f: &Fixture, client: &str) -> String {
        let response = consent_response(f, client, SECRET).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        assert!(!location.contains(SECRET));
        let redirect = Url::parse(location).unwrap();
        let pairs: HashMap<_, _> = redirect.query_pairs().into_owned().collect();
        assert_eq!(pairs["existing"], "yes");
        assert_eq!(pairs["state"], "some state&with=specials");
        assert_eq!(pairs["iss"], f.base);
        pairs["code"].clone()
    }

    async fn exchange(
        f: &Fixture,
        client: &str,
        code: &str,
        verifier: &str,
        resource: Option<&str>,
        redirect: &str,
    ) -> reqwest::Response {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", client),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ];
        if let Some(resource) = resource {
            form.push(("resource", resource));
        }
        f.client
            .post(format!("{}/token", f.base))
            .form(&form)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn discovery_pkce_consent_tokens_and_legacy_routes() {
        let f = fixture().await;
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
        ] {
            let metadata: serde_json::Value = f
                .client
                .get(format!("{}{path}", f.base))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(metadata["resource"], format!("{}/mcp", f.base));
            assert_eq!(metadata["authorization_servers"][0], f.base);
        }
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
            metadata["code_challenge_methods_supported"],
            json!(["S256"])
        );
        assert_eq!(
            metadata["registration_endpoint"],
            format!("{}/register", f.base)
        );
        assert_eq!(
            metadata["token_endpoint_auth_methods_supported"],
            json!(["none"])
        );
        assert!(!metadata.to_string().contains(SECRET));
        let client = register_client(&f).await;
        let code = issue_code(&f, &client).await;
        let response = exchange(
            &f,
            &client,
            &code,
            VERIFIER,
            Some(&format!("{}/mcp", f.base)),
            REDIRECT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let token: serde_json::Value = response.json().await.unwrap();
        assert_eq!(token["expires_in"], 3600);
        assert_eq!(token["token_type"], "Bearer");
        let access = token["access_token"].as_str().unwrap();
        assert_ne!(access, SECRET);
        for bearer in [SECRET, access] {
            assert_eq!(
                f.client
                    .post(format!("{}/mcp", f.base))
                    .bearer_auth(bearer)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert_eq!(
                f.client
                    .put(format!("{}/upload/test", f.base))
                    .bearer_auth(bearer)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        let replay = exchange(&f, &client, &code, VERIFIER, None, REDIRECT).await;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            replay.json::<serde_json::Value>().await.unwrap()["error"],
            "invalid_grant"
        );
        f.oauth
            .store
            .lock()
            .unwrap()
            .tokens
            .get_mut(&hash(access))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        for bearer in [None, Some("wrong"), Some(access)] {
            for path in ["/mcp", "/upload/test"] {
                let mut request = if path == "/mcp" {
                    f.client.post(format!("{}{path}", f.base))
                } else {
                    f.client.put(format!("{}{path}", f.base))
                };
                if let Some(bearer) = bearer {
                    request = request.bearer_auth(bearer);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                assert!(response.headers()[header::WWW_AUTHENTICATE]
                    .to_str()
                    .unwrap()
                    .contains(&format!(
                        "resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                        f.base
                    )));
            }
        }
        assert_eq!(
            f.client
                .post(format!("{}/mcp", f.base))
                .bearer_auth(SECRET)
                .header(header::ORIGIN, "https://evil.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn rejects_wrong_secret_csrf_and_canceled_consent() {
        let f = fixture().await;
        let client = register_client(&f).await;
        let response = consent_response(&f, &client, "wrong").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(f.oauth.store.lock().unwrap().codes.is_empty());
        let (nonce, cookie) = begin(&f, &client).await;
        for (origin, browser_cookie) in [
            ("https://evil.example", cookie.as_str()),
            (f.base.as_str(), "missing=nonce"),
        ] {
            let response = f
                .client
                .post(format!("{}/authorize", f.base))
                .header(header::ORIGIN, origin)
                .header(header::COOKIE, browser_cookie)
                .form(&[
                    ("request_id", nonce.as_str()),
                    ("password", SECRET),
                    ("decision", "allow"),
                ])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = f
            .client
            .post(format!("{}/authorize", f.base))
            .header(header::ORIGIN, &f.base)
            .header(header::COOKIE, &cookie)
            .form(&[("request_id", nonce.as_str()), ("decision", "deny")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .contains("error=access_denied"));
        assert!(f.oauth.store.lock().unwrap().codes.is_empty());
    }

    #[tokio::test]
    async fn rejects_invalid_pkce_client_redirect_resource_and_expired_code() {
        let f = fixture().await;
        let client = register_client(&f).await;
        for (key, value) in [
            ("redirect_uri", "https://evil.example/callback"),
            ("client_id", "unknown"),
            ("code_challenge_method", "plain"),
            ("code_challenge", "bad"),
            ("resource", "https://other.example/mcp"),
            ("scope", "admin"),
        ] {
            let mut query = params(&client, CHALLENGE);
            query.retain(|(k, _)| *k != key);
            query.push((key, value));
            let response = f
                .client
                .get(format!("{}/authorize", f.base))
                .query(&query)
                .send()
                .await
                .unwrap();
            if matches!(key, "redirect_uri" | "client_id") {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
                assert!(response.headers().get(header::LOCATION).is_none());
            } else {
                assert_eq!(response.status(), StatusCode::SEE_OTHER, "{key}");
                let location = response.headers()[header::LOCATION].to_str().unwrap();
                assert!(location.starts_with(REDIRECT));
                assert!(location.contains("error="));
            }
        }
        for (id, verifier, resource, redirect) in [
            (client.as_str(), "short", None, REDIRECT),
            (
                client.as_str(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                None,
                REDIRECT,
            ),
            ("other-client", VERIFIER, None, REDIRECT),
            (
                client.as_str(),
                VERIFIER,
                None,
                "https://client.example/other",
            ),
            (
                client.as_str(),
                VERIFIER,
                Some("https://other.example/mcp"),
                REDIRECT,
            ),
        ] {
            let code = issue_code(&f, &client).await;
            assert_eq!(
                exchange(&f, id, &code, verifier, resource, redirect)
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                exchange(&f, &client, &code, VERIFIER, None, REDIRECT)
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let code = issue_code(&f, &client).await;
        f.oauth
            .store
            .lock()
            .unwrap()
            .codes
            .get_mut(&hash(&code))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            exchange(&f, &client, &code, VERIFIER, None, REDIRECT)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn redirect_and_pkce_validation_follow_rfc7636() {
        assert!(valid_verifier(VERIFIER));
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
            CHALLENGE
        );
        assert!(valid_challenge(CHALLENGE));
        assert!(!valid_verifier(&"!".repeat(43)));
        assert!(!valid_verifier(&"x".repeat(129)));
        for uri in [
            REDIRECT,
            "http://127.0.0.1:1234/callback",
            "http://[::1]:1234/callback",
            "http://localhost:1234/callback",
        ] {
            assert!(valid_redirect(uri), "{uri}");
        }
        for uri in [
            "http://127.evil.example/cb",
            "http://remote.example/cb",
            "https://user:password@example.com/cb",
            "https://example.com/cb#frag",
            "javascript:alert(1)",
            "https://example.com/cb?code=bad",
        ] {
            assert!(!valid_redirect(uri), "{uri}");
        }
    }

    #[tokio::test]
    async fn registration_validation_and_capacity() {
        let f = fixture().await;
        for request in [
            json!({"redirect_uris": []}),
            json!({"redirect_uris": ["http://remote.example/cb"]}),
            json!({"redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_post"}),
            json!({"redirect_uris": [REDIRECT], "grant_types": ["password"]}),
        ] {
            assert_eq!(
                f.client
                    .post(format!("{}/register", f.base))
                    .json(&request)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        {
            let mut store = f.oauth.store.lock().unwrap();
            for i in 0..CAPACITY {
                store.clients.insert(
                    i.to_string(),
                    Client {
                        redirect_uris: vec![REDIRECT.into()],
                        allow_refresh: true,
                    },
                );
            }
        }
        assert_eq!(
            f.client
                .post(format!("{}/register", f.base))
                .json(&json!({"redirect_uris": [REDIRECT]}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn registered_clients_survive_restart_but_tokens_do_not() {
        let directory =
            std::env::temp_dir().join(format!("deep-obsidian-oauth-{}", generate_token()));
        let path = directory.join("oauth-clients.json");
        let config = OAuthConfig {
            issuer_url: "https://server.example".into(),
            access_token_ttl_seconds: 3600,
            refresh_token_ttl_seconds: deep_obsidian_types::default_oauth_refresh_ttl(),
        };
        let oauth = OAuthState::new(&config, "/custom-mcp", Some(path.clone())).unwrap();
        {
            let mut store = oauth.store.lock().unwrap();
            store.clients.insert(
                "test-client".into(),
                Client {
                    redirect_uris: vec![REDIRECT.into()],
                    allow_refresh: true,
                },
            );
            store.tokens.insert(
                hash("temporary-access"),
                AccessToken {
                    expires: Instant::now() + CODE_TTL,
                    family: None,
                },
            );
            oauth.save_clients(&store.clients).unwrap();
        }
        let restarted = OAuthState::new(&config, "/custom-mcp", Some(path.clone())).unwrap();
        assert!(restarted
            .store
            .lock()
            .unwrap()
            .clients
            .contains_key("test-client"));
        assert!(!restarted.accepts("temporary-access"));
        assert_eq!(restarted.resource, "https://server.example/custom-mcp");
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("temporary-access"));
        fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn malformed_token_and_registration_requests_are_oauth_errors() {
        let f = fixture().await;
        let response = f
            .client
            .post(format!("{}/token", f.base))
            .form(&[("grant_type", "authorization_code")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            "invalid_request"
        );
        let response = f
            .client
            .post(format!("{}/register", f.base))
            .json(&json!({"redirect_uris": "invalid"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"],
            "invalid_client_metadata"
        );
    }

    #[tokio::test]
    async fn concurrent_code_exchange_only_issues_one_access_token() {
        let f = fixture().await;
        let client = register_client(&f).await;
        let code = issue_code(&f, &client).await;
        let (first, second) = tokio::join!(
            exchange(&f, &client, &code, VERIFIER, None, REDIRECT),
            exchange(&f, &client, &code, VERIFIER, None, REDIRECT)
        );
        let mut statuses = vec![first.status().as_u16(), second.status().as_u16()];
        statuses.sort();
        assert_eq!(statuses, vec![200, 400]);
        assert_eq!(f.oauth.store.lock().unwrap().tokens.len(), 1);
    }

    #[tokio::test]
    async fn expired_consent_and_password_attempt_limit_are_enforced() {
        let f = fixture().await;
        let client = register_client(&f).await;
        let (nonce, cookie) = begin(&f, &client).await;
        f.oauth
            .store
            .lock()
            .unwrap()
            .pending
            .get_mut(&hash(&nonce))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        let response = f
            .client
            .post(format!("{}/authorize", f.base))
            .header(header::ORIGIN, &f.base)
            .header(header::COOKIE, cookie)
            .form(&[
                ("request_id", nonce.as_str()),
                ("password", SECRET),
                ("decision", "allow"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        for _ in 0..30 {
            assert_eq!(
                consent_response(&f, &client, "wrong").await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            consent_response(&f, &client, SECRET).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(f.oauth.store.lock().unwrap().codes.is_empty());
        f.oauth.store.lock().unwrap().failure_window = Instant::now() - Duration::from_secs(61);
        assert_eq!(
            consent_response(&f, &client, SECRET).await.status(),
            StatusCode::SEE_OTHER
        );
    }
    include!("oauth_refresh_tests.rs");
}
