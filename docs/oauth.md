# Remote OAuth authentication

Legacy `Authorization: Bearer <DEEP_OBSIDIAN_AUTH_TOKEN>` remains accepted on
`/mcp` and `/upload/{token}`. OAuth is additive and opt-in: existing configs and
environment overrides keep their current behavior, including the bare `Bearer`
challenge when OAuth is absent. Health and readiness remain unauthenticated.

## Enable legacy + OAuth

Run `deep-obsidian-mcp setup-service --wizard`, choose HTTP, enable authentication,
then choose **Legacy bearer + OAuth (MCP / ChatGPT)** and enter the public HTTPS
origin. The wizard preserves existing token references and defaults to the current
mode when editing an existing setup. Choosing stdio preserves the HTTP auth config.

For an existing configuration, add this block, keeping its existing `tokenRef`
and `allowedOrigins` if present:

```json
{
  "auth": {
    "enabled": true,
    "oauth": {
      "issuerUrl": "https://obsidian-mcp.example.com",
      "accessTokenTtlSeconds": 3600,
      "refreshTokenTtlSeconds": 2592000
    }
  }
}
```

This is an excerpt, not a complete vault configuration. Supply the existing
secret through `DEEP_OBSIDIAN_AUTH_TOKEN` or the existing keyring/encrypted-file
`tokenRef`. OAuth refuses to start without a resolved secret. No secret belongs
in this JSON, client registration metadata, or a plugin archive. Removing
`auth.oauth` returns to legacy-only mode; disabling authentication in the wizard
also removes OAuth configuration.

`issuerUrl` is the canonical external origin, without a path, credentials, query
or fragment. HTTPS is required except on localhost or loopback IPs for development.
The server never derives discovery URLs from untrusted forwarded headers. The
configured `http.mcpPath` determines the resource URL (normally `<issuer>/mcp`).
`accessTokenTtlSeconds` accepts 1–86400 seconds and defaults to 3600.
`refreshTokenTtlSeconds` defaults to 2592000 (30 days), accepts 0–2592000, and
zero disables refresh tokens. This is an absolute authorization lifetime from
the initial code exchange; rotation never extends it. Existing OAuth configs
without this field receive the 30-day default. The wizard preserves explicit
refresh lifetimes when editing an existing setup.

## Client flow

1. A missing, invalid or expired bearer returns `401` with
   `WWW-Authenticate: Bearer resource_metadata="<issuer>/.well-known/oauth-protected-resource", scope="obsidian"`.
2. Read `/.well-known/oauth-protected-resource` (also available at
   `/.well-known/oauth-protected-resource<http.mcpPath>`) and
   `/.well-known/oauth-authorization-server`.
3. Register a public client at `POST /register` with JSON `redirect_uris`,
   `token_endpoint_auth_method: "none"`, and optionally
   `grant_types: ["authorization_code", "refresh_token"]`, `response_types: ["code"]`.
   Omitting grant types enables both when refresh is configured. Explicitly
   requesting only `authorization_code` registers a client without refresh tokens.
   The response includes `client_id` and no client secret. Redirects must use HTTPS,
   except HTTP loopback callbacks, with no credentials or fragments. Only exact
   registered redirects are accepted. CIMD is not advertised or implemented.
4. Open `/authorize` with `response_type=code`, `client_id`, `redirect_uri`,
   `code_challenge`, `code_challenge_method=S256`, `state`, `scope=obsidian`, and
   `resource=<issuer><http.mcpPath>`. Unsupported scopes or resources are rejected.
   The owner reviews the client ID and redirect, enters the existing server secret
   directly into the server's password form, and explicitly allows access.
   Cancel returns `error=access_denied`. Redirects echo `state` and include `iss`.
5. Exchange the code at `POST /token` using `application/x-www-form-urlencoded`:
   `grant_type=authorization_code`, `client_id`, the same `redirect_uri`, `code`,
   `code_verifier`, and `resource`. The client never sends the owner's secret here.
   PKCE requires an RFC 7636 verifier and its SHA-256 base64url challenge.
6. Use the returned `access_token` as a bearer on `/mcp` or `/upload/{token}`.
   The response also includes a `refresh_token` and `refresh_token_expires_in`
   when enabled for this client. Tokens carry only the `obsidian` scope and are bound to this server's configured
   resource. The response includes `expires_in`, `scope`, and `resource`.

For older clients, omitted `scope` defaults to `obsidian` and omitted `resource`
defaults to the sole configured MCP resource. Explicitly different values are
rejected. There is no password or client-credentials grant.

## Returning from consent

Callbacks with a domain name retain the HTTP 303 redirect after Allow or Cancel.
For an IP-literal callback, the POST instead returns a same-origin HTML page
(HTTP 200) that immediately navigates to the exact registered callback using a
meta refresh. A Continue link is available if automatic navigation is disabled.
This separates the callback GET from the form submission, which WebKit can
otherwise block for IP literals. The page requires no JavaScript, keeps
`form-action 'self'`, disables scripts and framing, and uses `no-store` and
`no-referrer`. It never contains or forwards the owner's secret.

This remains the authorization-code flow: callback URL, `code`/`error`, `state`,
`iss`, PKCE, and the token exchange are unchanged. A program that directly posts
the consent form must handle the HTML response for IP callbacks; that form is
a browser interaction, not a token API.
[OAuth permits returning through means provided by the user-agent](https://www.rfc-editor.org/rfc/rfc6749#section-4.1.1),
and [loopback IP callbacks are the native-app pattern](https://www.rfc-editor.org/rfc/rfc8252#section-7.3).

## Browser regression tests

The consent suite drives the real Rust CLI through a local HTTPS proxy with
Chromium, Firefox and WebKit. It covers Allow and Cancel with an HTTPS callback
on another origin, HTTP localhost, and IPv4/IPv6 loopback callbacks, wrong owner credentials,
PKCE exchange, MCP access, and rejection of code reuse. Callback requests must
be GETs with an empty body: the owner secret must never leave the authorization
server. Browser headers and CSP are not rewritten or mocked.

```sh
cargo build --locked -p deep-obsidian-cli
cd tests/browser
npm ci
npx playwright install --only-shell chromium firefox webkit
npm test
```

Node.js 20+ and OpenSSL are required. Linux hosts may need
`npx playwright install --with-deps --only-shell chromium firefox webkit`.
`DEEP_OBSIDIAN_TEST_BINARY` can select a previously built CLI binary.
Each worker creates a temporary vault, a fake owner secret, and a self-signed
certificate. No production vault, keychain or system certificate trust is used.
Failure traces go to `output/playwright/`; CI uploads them for seven days.
The WebKit project exercises Playwright's WebKit build, not the installed Safari
application or the real ChatGPT callback. IPv4 and IPv6 use real listeners
on `127.0.0.1` and `::1`. Set `DEEP_OBSIDIAN_BROWSER_NO_JS=1` to rerun with
JavaScript disabled. The IP callback page is also tested for cookie/nonce/CSRF
enforcement, exact callback parameters, security headers, and one-use consent.
On macOS, Firefox's app-data directory is isolated with `CFFIXED_USER_HOME` to
avoid accessing the user's Firefox data ([upstream issue](https://github.com/microsoft/playwright/issues/42768)).

## Refresh tokens

After access-token expiry, the client sends `POST /token` with form fields
`grant_type=refresh_token`, `client_id`, and `refresh_token`. It may include the
same `resource` and `scope=obsidian`; other resources or scopes are rejected.
Neither the owner's password nor a new browser consent is needed for renewal.
The client receives a new access token and a new refresh token and must replace
its stored refresh credential. Refresh tokens are never accepted as MCP bearers.

The initial authorization expires after 30 days by default; renewing does not
reset that deadline, and access tokens never outlive it. An expired or unknown
refresh token returns `invalid_grant`. Reusing a spent refresh token revokes the
entire authorization family, including its current refresh token and every
still-valid access token. Other authorizations and legacy bearer access remain
valid. Clients must serialize refresh requests: concurrent reuse also triggers
revocation. There is no replay grace window.

Refresh families and the hashes of current and spent refresh tokens stay in
bounded memory (1024 families, 8192 refresh hashes overall). At capacity,
renewal returns `temporarily_unavailable` without consuming the current refresh
token. Expired families are pruned. After authorization expiry, replay revocation
or server restart, the client must repeat browser authorization.

## Operation and security boundaries

Codes and consent requests expire after five minutes; each code is consumed once,
including failed exchanges. Access tokens expire at the configured lifetime.
Codes, consent cookies, access tokens and refresh tokens are stored as hashes in bounded memory
stores. Authorization requires a browser cookie plus a form nonce and an exact
same-origin POST. The page disallows framing and scripts, and OAuth responses use
`Cache-Control: no-store`. Thirty failed password validations within a minute
temporarily block further consent submissions. Registration and grant stores
each cap at 1024 entries; registration is public and should also be rate-limited
at the proxy for Internet deployments.

Registered public clients persist in `<indexDir>/oauth-clients.json` (mode 0600 on
Unix) using atomic replacement. This file contains IDs and allowed redirects, no
owner secrets or bearer tokens. Keep the index directory private and persistent.
Codes, access tokens and refresh tokens are invalidated on server restart, and the client must
reauthorize using its existing registered ID. Removing the client registry and
restarting requires clients to register again. Removing a specific registered
client and restarting revokes that client's tokens as all bearer tokens are
cleared on restart.

Run one server instance; tokens are not shared across replicas. Publish the
discovery endpoints, `/register`, `/authorize` and `/token` through the same HTTPS
proxy as `/mcp`, and preserve browser `Origin` and cookies. Existing protected-route
Origin validation still applies to both token types. Browser MCP clients require
an appropriate `auth.allowedOrigins` entry; server-to-server clients usually omit
`Origin`. Upload links still need their one-use upload ticket in addition to a
valid bearer. Legacy bearer access remains non-expiring until the owner rotates
the shared secret. Rotation requires restarting the service, which also
invalidates all issued OAuth tokens.

Docker's `DO_AUTH_MODE=legacy+oauth`, `DO_OAUTH_ISSUER_URL`, and optional
`DO_OAUTH_ACCESS_TOKEN_TTL_SECONDS` and `DO_OAUTH_REFRESH_TOKEN_TTL_SECONDS` generate this config on first boot for any
root backend. Existing or mounted configs remain authoritative. See
[Docker deployment](docker.md#optional-oauth-for-mcp--chatgpt).

References: [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization),
[OpenAI plugin authentication](https://developers.openai.com/plugins/build/auth),
[PKCE RFC 7636](https://www.rfc-editor.org/rfc/rfc7636),
[Protected Resource Metadata RFC 9728](https://www.rfc-editor.org/rfc/rfc9728),
[Authorization Server Metadata RFC 8414](https://www.rfc-editor.org/rfc/rfc8414).
