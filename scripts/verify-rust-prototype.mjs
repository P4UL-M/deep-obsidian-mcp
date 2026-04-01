#!/usr/bin/env node

import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { verifyServiceHttp } from "./verify-service-http.mjs";

const ROOT_DIR = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_VAULT = path.join(ROOT_DIR, "tests", "fixtures", "vault");
const DEFAULT_MANIFEST = path.join(ROOT_DIR, "rust", "Cargo.toml");
const DEFAULT_BINARY = path.join(ROOT_DIR, "rust", "target", "debug", "deep-obsidian-cli");

function parseArgs(argv) {
  const args = [...argv];
  const out = {
    launcher: "cargo",
    manifestPath: DEFAULT_MANIFEST,
    binaryPath: DEFAULT_BINARY,
    package: "deep-obsidian-cli",
    binName: "deep-obsidian-cli",
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

function spawnService({ launcher, manifestPath, binaryPath, package: packageName, binName, vault, indexDir, host, port, mcpPath, healthPath, extraArgs }) {
  const isCargo = launcher === "cargo";
  const command = isCargo ? "cargo" : binaryPath;
  const args = isCargo
    ? ["run", "--manifest-path", manifestPath, "--package", packageName, "--bin", binName, "--", "serve"]
    : ["serve"];

  args.push("--transport", "http", "--host", host, "--port", String(port), "--mcp-path", mcpPath, "--health-path", healthPath, "--vault", vault);
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
    const summary = await verifyServiceHttp({
      url: `http://${args.host}:${port}${args.mcpPath}`,
      healthUrl: `http://${args.host}:${port}${args.healthPath}`,
      vault: resolvedVault,
      expectVault: resolvedExpectVault,
    });
    console.log(JSON.stringify({
      launcher: args.launcher,
      manifestPath: resolvedManifest,
      binaryPath: resolvedBinary,
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

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.stack ?? error.message : String(error));
    process.exit(1);
  });
}
