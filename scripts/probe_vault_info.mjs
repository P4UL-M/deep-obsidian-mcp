import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

function wait(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function main() {
  const vaultPath = process.argv[2];
  if (!vaultPath) {
    throw new Error("Usage: node scripts/probe_vault_info.mjs <vault-path>");
  }

  const child = spawn("node", ["dist/index.js", vaultPath, "--stdio-mode", "newline"], {
    cwd: fileURLToPath(new URL("..", import.meta.url)),
    stdio: ["pipe", "pipe", "pipe"],
  });

  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });

  const readJsonLine = async (timeoutMs = 8000) => {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      const newlineIndex = stdout.indexOf("\n");
      if (newlineIndex >= 0) {
        const line = stdout.slice(0, newlineIndex).trim();
        stdout = stdout.slice(newlineIndex + 1);
        if (line) {
          return JSON.parse(line);
        }
      }
      if (child.exitCode !== null) {
        throw new Error(`Server exited early with code ${child.exitCode}. stderr=${stderr}`);
      }
      await wait(25);
    }
    throw new Error(`Timed out waiting for server response. stderr=${stderr}`);
  };

  child.stdin.write(
    `${JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-03-26",
        capabilities: {},
        clientInfo: { name: "probe", version: "0.0.0" },
      },
    })}\n`,
  );
  await readJsonLine();

  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
  child.stdin.write(
    `${JSON.stringify({
      jsonrpc: "2.0",
      id: 2,
      method: "tools/call",
      params: { name: "vault_info", arguments: {} },
    })}\n`,
  );
  const response = await readJsonLine();

  const structured = JSON.parse(response.result.content[0].text);
  console.log(JSON.stringify({
    autoReindex: structured.autoReindex,
    reindexDebounceMs: structured.reindexDebounceMs,
    reindexIntervalMs: structured.reindexIntervalMs,
    markdownFileCount: structured.markdownFileCount,
  }));

  child.kill("SIGTERM");
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
