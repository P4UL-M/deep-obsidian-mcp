#!/usr/bin/env node

import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { verifyServiceHttp } from "./verify-service-http.mjs";

const ROOT_DIR = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_VAULT = path.join(ROOT_DIR, "tests", "fixtures", "vault");
const DEFAULT_ENTRYPOINT = path.join(ROOT_DIR, "dist", "index.js");

function parseArgs(argv) {
  const args = [...argv];
  const out = {
    command: "node",
    entrypoint: DEFAULT_ENTRYPOINT,
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
    if (current === "--command") {
      out.command = args.shift() ?? out.command;
      continue;
    }
    if (current === "--entrypoint") {
      out.entrypoint = args.shift() ?? out.entrypoint;
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

function spawnService({ command, entrypoint, vault, indexDir, host, port, mcpPath, healthPath, extraArgs }) {
  const args = entrypoint ? [entrypoint, vault] : [vault];
  args.push("--transport", "http", "--host", host, "--port", String(port), "--mcp-path", mcpPath, "--health-path", healthPath);
  if (indexDir) {
    args.push("--index-dir", indexDir);
  }
  args.push(...extraArgs);

  return spawn(command, args, {
    cwd: ROOT_DIR,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env },
  });
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const resolvedVault = path.resolve(ROOT_DIR, args.vault);
  const resolvedEntrypoint = path.resolve(ROOT_DIR, args.entrypoint);
  const resolvedIndexDir = args.indexDir ? path.resolve(ROOT_DIR, args.indexDir) : undefined;
  const resolvedExpectVault = args.expectVault ? path.resolve(ROOT_DIR, args.expectVault) : resolvedVault;

  await access(resolvedVault);

  const port = args.port ?? await getFreePort(args.host);
  const child = spawnService({
    ...args,
    port,
    vault: resolvedVault,
    entrypoint: resolvedEntrypoint,
    indexDir: resolvedIndexDir,
  });
  const exitPromise = new Promise((resolve) => child.once("exit", resolve));

  try {
    const summary = await verifyServiceHttp({
      url: `http://${args.host}:${port}${args.mcpPath}`,
      healthUrl: `http://${args.host}:${port}${args.healthPath}`,
      vault: resolvedVault,
      expectVault: resolvedExpectVault,
    });
    console.log(JSON.stringify({
      command: args.command,
      entrypoint: resolvedEntrypoint,
      vault: resolvedVault,
      port,
      healthPath: args.healthPath,
      mcpPath: args.mcpPath,
      status: "ok",
      summary,
    }, null, 2));
  } finally {
    child.kill("SIGTERM");
    await Promise.race([
      exitPromise,
      new Promise((resolve) => setTimeout(resolve, 5000)),
    ]);
    if (child.exitCode === null) {
      child.kill("SIGKILL");
      await exitPromise.catch(() => undefined);
    }
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  process.exit(1);
});
