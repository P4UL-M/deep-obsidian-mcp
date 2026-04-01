#!/usr/bin/env node

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";

const serverUrl = process.argv[2] ?? process.env.DEEP_OBSIDIAN_URL ?? "http://127.0.0.1:4100/mcp";

const client = new Client({
  name: "deep-obsidian-probe",
  version: "1.0.0",
});

const transport = new StreamableHTTPClientTransport(new URL(serverUrl));

try {
  await client.connect(transport);
  const tools = await client.listTools();
  const vaultInfo = await client.callTool({ name: "vault_info", arguments: {} });
  console.log(JSON.stringify({
    serverUrl,
    toolCount: tools.tools.length,
    firstTool: tools.tools[0]?.name ?? null,
    resultPreview: vaultInfo.content[0]?.type === "text" ? vaultInfo.content[0].text : null,
  }, null, 2));
} finally {
  await transport.close().catch(() => undefined);
  await client.close().catch(() => undefined);
}
