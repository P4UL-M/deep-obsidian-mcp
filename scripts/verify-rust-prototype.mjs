#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT_DIR = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_VAULT = path.join(ROOT_DIR, "tests", "fixtures", "vault");
const DEFAULT_MANIFEST = path.join(ROOT_DIR, "rust", "Cargo.toml");
const DEFAULT_BINARY = path.join(ROOT_DIR, "rust", "target", "debug", "deep-obsidian-mcp");
const HOMEBREW_CARGO = "/opt/homebrew/opt/rustup/bin/cargo";

function parseArgs(argv) {
  const args = [...argv];
  const out = {
    launcher: "cargo",
    manifestPath: DEFAULT_MANIFEST,
    binaryPath: DEFAULT_BINARY,
    package: "deep-obsidian-cli",
    binName: "deep-obsidian-mcp",
    vault: DEFAULT_VAULT,
    indexDir: undefined,
    host: "127.0.0.1",
    port: undefined,
    mcpPath: "/mcp",
    healthPath: "/healthz",
    expectVault: undefined,
    extraArgs: [],
  };

  while (args.length > 0) {
    const current = args.shift();
    if (current === "--launcher") {
      out.launcher = args.shift() ?? out.launcher;
      continue;
    }
    if (current === "--manifest-path") {
      out.manifestPath = args.shift() ?? out.manifestPath;
      continue;
    }
    if (current === "--binary") {
      out.binaryPath = args.shift() ?? out.binaryPath;
      continue;
    }
    if (current === "--package") {
      out.package = args.shift() ?? out.package;
      continue;
    }
    if (current === "--bin") {
      out.binName = args.shift() ?? out.binName;
      continue;
    }
    if (current === "--vault") {
      out.vault = args.shift() ?? out.vault;
      continue;
    }
    if (current === "--index-dir") {
      out.indexDir = args.shift();
      continue;
    }
    if (current === "--host") {
      out.host = args.shift() ?? out.host;
      continue;
    }
    if (current === "--port") {
      out.port = Number(args.shift());
      continue;
    }
    if (current === "--mcp-path") {
      out.mcpPath = args.shift() ?? out.mcpPath;
      continue;
    }
    if (current === "--health-path") {
      out.healthPath = args.shift() ?? out.healthPath;
      continue;
    }
    if (current === "--expect-vault") {
      out.expectVault = args.shift();
      continue;
    }
    if (current === "--") {
      out.extraArgs.push(...args);
      break;
    }
  }

  return out;
}

function resolveCargoCommand() {
  if (process.env.CARGO) {
    return process.env.CARGO;
  }
  if (fs.existsSync(HOMEBREW_CARGO)) {
    return HOMEBREW_CARGO;
  }
  return "cargo";
}

async function getFreePort(host) {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, host, () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close(() => reject(new Error("failed to allocate a port")));
        return;
      }
      const port = address.port;
      server.close((error) => {
        if (error) {
          reject(error);
          return;
        }
        resolve(port);
      });
    });
  });
}

function spawnService({
  launcher,
  manifestPath,
  binaryPath,
  package: packageName,
  binName,
  vault,
  indexDir,
  host,
  port,
  mcpPath,
  healthPath,
  extraArgs,
}) {
  const isCargo = launcher === "cargo";
  const command = isCargo ? resolveCargoCommand() : binaryPath;
  const args = isCargo
    ? ["run", "--manifest-path", manifestPath, "--package", packageName, "--bin", binName, "--", "serve"]
    : ["serve"];

  args.push(
    "--transport",
    "http",
    "--host",
    host,
    "--port",
    String(port),
    "--mcp-path",
    mcpPath,
    "--health-path",
    healthPath,
    "--vault",
    vault,
  );
  if (indexDir) {
    args.push("--index-dir", indexDir);
  }
  args.push(...extraArgs);

  const env = { ...process.env };
  if (isCargo && fs.existsSync(HOMEBREW_CARGO)) {
    env.PATH = `/opt/homebrew/opt/rustup/bin:${env.PATH ?? ""}`;
  }

  return spawn(command, args, {
    cwd: ROOT_DIR,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });
}

async function waitForHealth(healthUrl, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(healthUrl);
      if (response.ok) {
        return response;
      }
      lastError = new Error(`health check returned ${response.status}`);
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw lastError ?? new Error(`timed out waiting for ${healthUrl}`);
}

async function jsonRpc(url, payload) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  assert.equal(response.status, 200, `unexpected MCP HTTP status: ${response.status}`);
  return await response.json();
}

function readToolPayload(response) {
  const text = response?.result?.content?.find?.((item) => item.type === "text" || item.kind === "text")?.text;
  if (typeof text === "string") {
    return JSON.parse(text);
  }
  return response?.result?.structuredContent ?? response?.result?.structured_content;
}

async function verifyRustPrototype({ url, healthUrl, expectVault }) {
  const healthResponse = await waitForHealth(healthUrl);
  const health = await healthResponse.json();
  assert.equal(health.status, "ok", "health endpoint did not report ok");
  assert.equal(health.vaultPath, expectVault, "health vaultPath mismatch");

  const toolsList = await jsonRpc(url, {
    jsonrpc: "2.0",
    id: 1,
    method: "tools/list",
    params: {},
  });
  const toolNames = (toolsList?.result?.tools ?? []).map((tool) => tool.name);
  for (const requiredTool of ["vault_info", "read_file"]) {
    assert.ok(toolNames.includes(requiredTool), `missing MCP tool: ${requiredTool}`);
  }

  const vaultInfoResponse = await jsonRpc(url, {
    jsonrpc: "2.0",
    id: 2,
    method: "tools/call",
    params: { name: "vault_info", arguments: {} },
  });
  const vaultInfo = readToolPayload(vaultInfoResponse);
  assert.equal(vaultInfo.vaultPath, expectVault, "vault_info vaultPath mismatch");
  assert.ok(vaultInfo.markdownFileCount > 0, "vault_info returned an empty vault");

  const readFileResponse = await jsonRpc(url, {
    jsonrpc: "2.0",
    id: 3,
    method: "tools/call",
    params: { name: "read_file", arguments: { path: "Home.md" } },
  });
  const readFile = readToolPayload(readFileResponse);
  assert.ok(
    typeof readFile.text === "string" && readFile.text.includes("Projects/Brew Service"),
    "read_file did not return fixture content",
  );

  return {
    url,
    healthUrl,
    toolCount: toolNames.length,
    tools: toolNames,
    vaultPath: vaultInfo.vaultPath,
  };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const resolvedVault = path.resolve(ROOT_DIR, args.vault);
  const resolvedManifest = path.resolve(ROOT_DIR, args.manifestPath);
  const resolvedBinary = path.resolve(ROOT_DIR, args.binaryPath);
  const resolvedIndexDir = args.indexDir ? path.resolve(ROOT_DIR, args.indexDir) : undefined;
  const resolvedExpectVault = args.expectVault ? path.resolve(ROOT_DIR, args.expectVault) : resolvedVault;

  if (args.launcher === "cargo") {
    await access(resolvedManifest);
  } else {
    await access(resolvedBinary);
  }
  await access(resolvedVault);

  const port = args.port ?? await getFreePort(args.host);
  const child = spawnService({
    ...args,
    port,
    vault: resolvedVault,
    manifestPath: resolvedManifest,
    binaryPath: resolvedBinary,
    indexDir: resolvedIndexDir,
  });
  const exitPromise = new Promise((resolve) => child.once("exit", resolve));

  try {
    const summary = await verifyRustPrototype({
      url: `http://${args.host}:${port}${args.mcpPath}`,
      healthUrl: `http://${args.host}:${port}${args.healthPath}`,
      expectVault: resolvedExpectVault,
    });
    console.log(
      JSON.stringify(
        {
          launcher: args.launcher,
          manifestPath: resolvedManifest,
          binaryPath: resolvedBinary,
          vault: resolvedVault,
          port,
          healthPath: args.healthPath,
          mcpPath: args.mcpPath,
          status: "ok",
          summary,
        },
        null,
        2,
      ),
    );
  } finally {
    child.kill("SIGTERM");
    await Promise.race([exitPromise, new Promise((resolve) => setTimeout(resolve, 5000))]);
    if (child.exitCode === null) {
      child.kill("SIGKILL");
      await exitPromise.catch(() => undefined);
    }
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.stack ?? error.message : String(error));
    process.exit(1);
  });
}
