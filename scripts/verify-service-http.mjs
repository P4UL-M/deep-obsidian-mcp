#!/usr/bin/env node

import assert from "node:assert/strict";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";

function parseArgs(argv) {
  const args = [...argv];
  const out = {
    url: "http://127.0.0.1:4100/mcp",
    healthUrl: undefined,
    vault: undefined,
    expectVault: undefined,
  };

  while (args.length > 0) {
    const current = args.shift();
    if (current === "--url") {
      out.url = args.shift() ?? out.url;
      continue;
    }
    if (current === "--health-url") {
      out.healthUrl = args.shift();
      continue;
    }
    if (current === "--vault") {
      out.vault = args.shift();
      continue;
    }
    if (current === "--expect-vault") {
      out.expectVault = args.shift();
      continue;
    }
  }

  if (!out.healthUrl) {
    const url = new URL(out.url);
    out.healthUrl = new URL("/healthz", url.origin).toString();
  }

  return out;
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

function readJsonToolResult(result) {
  const text = result.content?.find((item) => item.type === "text")?.text;
  if (text) {
    return JSON.parse(text);
  }
  if (result.structuredContent) {
    return result.structuredContent;
  }
  throw new Error("tool result did not contain JSON text or structured content");
}

export async function verifyServiceHttp({ url, healthUrl, vault, expectVault }) {
  const healthResponse = await waitForHealth(healthUrl);
  const health = await healthResponse.json();
  assert.equal(health.status, "ok", "health endpoint did not report ok");
  assert.ok(health.vaultPath, "health payload is missing vaultPath");

  const client = new Client({ name: "deep-obsidian-verifier", version: "1.0.0" });
  const transport = new StreamableHTTPClientTransport(new URL(url));

  try {
    await client.connect(transport);
    const tools = await client.listTools();
    const toolNames = tools.tools.map((tool) => tool.name);
    for (const requiredTool of ["vault_info", "read_file", "graph_traverse"]) {
      assert.ok(toolNames.includes(requiredTool), `missing MCP tool: ${requiredTool}`);
    }

    const vaultInfo = readJsonToolResult(await client.callTool({ name: "vault_info", arguments: {} }));
    assert.ok(vaultInfo.markdownFileCount > 0, "vault_info returned an empty vault");
    if (expectVault) {
      assert.equal(vaultInfo.vaultPath, expectVault, "vault_info vaultPath mismatch");
    }

    if (vault) {
      const readFile = readJsonToolResult(await client.callTool({ name: "read_file", arguments: { path: "Home.md" } }));
      const readText = typeof readFile.text === "string" ? readFile.text : JSON.stringify(readFile);
      assert.ok(readText.includes("Projects/Brew Service"), "read_file did not return fixture content");

      const graph = readJsonToolResult(await client.callTool({ name: "graph_traverse", arguments: { path: "Home.md", direction: "both", depth: 1, limit: 20 } }));
      assert.ok(graph.nodeCount >= 2, "graph traversal returned too few nodes");
      assert.ok(graph.edgeCount >= 2, "graph traversal returned too few edges");
    }

    return {
      url,
      healthUrl,
      toolCount: tools.tools.length,
      vaultPath: vaultInfo.vaultPath,
    };
  } finally {
    await transport.close().catch(() => undefined);
    await client.close().catch(() => undefined);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const summary = await verifyServiceHttp(args);
  console.log(JSON.stringify(summary, null, 2));
}

main().catch((error) => {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  process.exit(1);
});
