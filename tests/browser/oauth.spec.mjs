import { test, expect } from './fixture.mjs';

const verifier = 'dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk';
const challenge = 'E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM';
const state = 'browser-state&with=specials';

test.beforeEach(async ({ page }) => {
  page.on('console', message => {
    if (message.type() === 'error') console.log(`Browser: ${message.text()}`);
  });
  page.on('requestfailed', request => console.log(`Navigation failure: ${request.failure()?.errorText}`));
});

async function begin(page, request, oauth, redirect) {
  const registration = await request.post(`${oauth.issuer}/register`, {
    data: { redirect_uris: [redirect], token_endpoint_auth_method: 'none' },
  });
  expect(registration.status()).toBe(201);
  const client = (await registration.json()).client_id;
  const url = new URL('/authorize', oauth.issuer);
  url.search = new URLSearchParams({ response_type: 'code', client_id: client,
    redirect_uri: redirect, code_challenge: challenge, code_challenge_method: 'S256',
    state, scope: 'obsidian', resource: `${oauth.issuer}/mcp` }).toString();
  const response = await page.goto(url.toString());
  expect(response.status()).toBe(200);
  await expect(page.getByRole('heading', { name: 'Allow access to Deep Obsidian?' })).toBeVisible();
  return client;
}

for (const kind of ['https', 'loopback', 'ipv4', 'ipv6']) {
  for (const decision of ['allow', 'deny']) {
    test(`${decision} consent follows the ${kind} callback in the browser`, async ({ page, request, oauth }) => {
      const redirect = oauth.redirects[kind];
      const client = await begin(page, request, oauth, redirect);
      if (decision === 'allow') await page.getByLabel('Server secret').fill(oauth.secret);
      const callbackCount = oauth.callbacks.length;
      const post = page.waitForResponse(response => response.url() === `${oauth.issuer}/authorize`
        && response.request().method() === 'POST', { timeout: 10_000 });
      await page.getByRole('button', { name: decision === 'allow' ? 'Allow access' : 'Cancel', exact: true }).click();
      const consentResponse = await post;
      const ipCallback = kind === 'ipv4' || kind === 'ipv6';
      expect(consentResponse.status()).toBe(ipCallback ? 200 : 303);
      if (ipCallback) {
        expect(consentResponse.headers()['content-security-policy']).toBe(
          "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'");
        expect(consentResponse.headers()['cache-control']).toBe('no-store');
      }
      await expect(page.getByRole('heading', { name: 'Callback received' })).toBeVisible();
      const result = new URL(page.url());
      expect(result.origin).toBe(new URL(redirect).origin);
      expect(result.searchParams.get('state')).toBe(state);
      expect(result.searchParams.get('iss')).toBe(oauth.issuer);
      if (kind === 'https') expect(result.searchParams.get('next')).toBe('https://other.example');
      expect(oauth.callbacks).toHaveLength(callbackCount + 1);
      const received = oauth.callbacks.at(-1);
      expect(received.method).toBe('GET');
      expect(received.body).toBe('');
      expect(received.url).not.toContain(oauth.secret);
      expect(result.searchParams.has('password')).toBe(false);
      if (decision === 'deny') {
        expect(result.searchParams.get('error')).toBe('access_denied');
        expect(result.searchParams.has('code')).toBe(false);
      } else {
        const code = result.searchParams.get('code');
        expect(code).toBeTruthy();
        expect(result.searchParams.has('error')).toBe(false);
        const token = await request.post(`${oauth.issuer}/token`, { form: {
          grant_type: 'authorization_code', client_id: client, redirect_uri: redirect,
          code, code_verifier: verifier, resource: `${oauth.issuer}/mcp`,
        } });
        expect(token.status()).toBe(200);
        const credentials = await token.json();
        const mcp = await request.post(`${oauth.issuer}/mcp`, {
          headers: { Authorization: `Bearer ${credentials.access_token}` },
          data: { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
        });
        expect(mcp.status()).toBe(200);
        expect((await mcp.json()).result.serverInfo.name).toBeTruthy();
        const replay = await request.post(`${oauth.issuer}/token`, { form: {
          grant_type: 'authorization_code', client_id: client, redirect_uri: redirect,
          code, code_verifier: verifier,
        } });
        expect(replay.status()).toBe(400);
        expect((await replay.json()).error).toBe('invalid_grant');
      }
    });
  }
}

test('wrong owner secret never reaches the callback', async ({ page, request, oauth }) => {
  await begin(page, request, oauth, oauth.redirects.https);
  const callbackCount = oauth.callbacks.length;
  await page.getByLabel('Server secret').fill('wrong-secret');
  const post = page.waitForResponse(response => response.request().method() === 'POST');
  await page.getByRole('button', { name: 'Allow access', exact: true }).click();
  expect((await post).status()).toBe(401);
  await expect(page.locator('body')).toContainText('access_denied');
  expect(oauth.callbacks).toHaveLength(callbackCount);
});
