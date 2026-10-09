import { test as base, expect } from '@playwright/test';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { once } from 'node:events';

const root = fileURLToPath(new URL('../../', import.meta.url));
const secret = 'fake-browser-test-owner-secret';
const connections = new WeakMap();

async function listen(server, host = '127.0.0.1') {
  const sockets = new Set();
  connections.set(server, sockets);
  server.on('connection', socket => {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
  });
  server.listen(0, host);
  await once(server, 'listening');
  return server.address().port;
}

async function close(server) {
  if (!server.listening) return;
  const closed = new Promise(resolve => server.close(resolve));
  server.closeAllConnections();
  // Browser preconnects can leave TLS sockets without an HTTP request.
  for (const socket of connections.get(server) ?? []) socket.destroy();
  await closed;
}

export const test = base.extend({
  oauth: [async ({}, use) => {
    const directory = await mkdtemp(path.join(tmpdir(), 'deep-obsidian-browser-'));
    const servers = [];
    let child;
    let log = '';
    try {
      const vault = path.join(directory, 'vault');
      await mkdir(vault);
      const keyPath = path.join(directory, 'key.pem');
      const certPath = path.join(directory, 'cert.pem');
      execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes',
        '-keyout', keyPath, '-out', certPath, '-days', '1', '-subj', '/CN=localhost'],
      { stdio: 'ignore' });
      const tls = { key: await readFile(keyPath), cert: await readFile(certPath) };
      const reservation = http.createServer();
      const backendPort = await listen(reservation);
      await close(reservation);

      // Real HTTPS proxy: preserves Origin/Fetch Metadata/CSP and performs no rewriting.
      const proxy = https.createServer(tls, (request, response) => {
        const upstream = http.request({ hostname: '127.0.0.1', port: backendPort,
          path: request.url, method: request.method, headers: request.headers }, result => {
          response.writeHead(result.statusCode, result.headers);
          result.pipe(response);
        });
        upstream.on('error', () => { response.writeHead(502); response.end(); });
        request.pipe(upstream);
      });
      servers.push(proxy);
      const issuer = `https://localhost:${await listen(proxy)}`;
      const callbacks = [];
      const callback = (request, response) => {
        if (new URL(request.url, 'http://localhost').pathname !== '/callback') {
          response.writeHead(404);
          response.end();
          return;
        }
        let body = '';
        request.on('data', chunk => { body += chunk; });
        request.on('end', () => {
          callbacks.push({ method: request.method, url: request.url, body });
          response.writeHead(200, { 'content-type': 'text/html' });
          response.end('<!doctype html><title>OAuth callback</title><h1>Callback received</h1>');
        });
      };
      const httpsCallback = https.createServer(tls, callback);
      const httpCallback = http.createServer(callback);
      const ipv6Callback = http.createServer(callback);
      servers.push(httpsCallback, httpCallback, ipv6Callback);
      const httpPort = await listen(httpCallback);
      const redirects = {
        https: `https://localhost:${await listen(httpsCallback)}/callback?next=https://other.example`,
        loopback: `http://localhost:${httpPort}/callback`,
        ipv4: `http://127.0.0.1:${httpPort}/callback`,
        ipv6: `http://[::1]:${await listen(ipv6Callback, '::1')}/callback`,
      };
      const configPath = path.join(directory, 'config.json');
      await writeFile(configPath, JSON.stringify({ vaultPath: vault,
        indexDir: path.join(directory, 'index'), transport: 'http',
        http: { host: '127.0.0.1', port: backendPort }, autoReindex: { enabled: false },
        auth: { enabled: true, oauth: { issuerUrl: issuer } },
      }));
      const env = { ...process.env, DEEP_OBSIDIAN_AUTH_TOKEN: secret,
        XDG_CONFIG_HOME: path.join(directory, 'config') };
      delete env.DEEP_OBSIDIAN_ALLOW_INSECURE;
      child = spawn(process.env.DEEP_OBSIDIAN_TEST_BINARY || path.join(root, 'target/debug/deep-obsidian-mcp'),
        ['--config', configPath, 'serve'], { env, stdio: ['ignore', 'pipe', 'pipe'] });
      child.stdout.on('data', chunk => { log += chunk; });
      child.stderr.on('data', chunk => { log += chunk; });
      let spawnError;
      child.on('error', error => { spawnError = error; });
      await expect.poll(async () => {
        if (spawnError) throw spawnError;
        if (child.exitCode !== null) throw new Error(`Server exited: ${log}`);
        try { return (await fetch(`http://127.0.0.1:${backendPort}/healthz`)).status; }
        catch { return 0; }
      }, { timeout: 10_000 }).toBe(200);
      await use({ issuer, secret, redirects, callbacks });
    } finally {
      if (child && child.exitCode === null && child.signalCode === null && child.pid) {
        const exited = once(child, 'exit');
        child.kill('SIGKILL');
        await exited;
      }
      await Promise.all(servers.map(close));
      await rm(directory, { recursive: true, force: true });
    }
  }, { scope: 'worker' }],
});
export { expect };
