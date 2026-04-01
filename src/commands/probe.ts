import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";

import { probeHealthUrl } from "./shared.js";
import {
  buildServiceEndpoints,
  ensureHttpServiceConfig,
  normalizeServiceConfig,
  type ServiceConfigInput,
  type ResolvedServiceConfig,
} from "../service.js";

export interface ProbeOptions {
  config: ServiceConfigInput;
  timeoutMs?: number;
}

export interface HealthProbeResult {
  ok: boolean;
  status?: number;
  body?: unknown;
  error?: string;
}

export interface McpProbeResult {
  ok: boolean;
  toolCount?: number;
  firstTool?: string | null;
  vaultInfo?: unknown;
  error?: string;
}

export interface ProbeResult {
  endpoints: ReturnType<typeof buildServiceEndpoints>;
  health: HealthProbeResult;
  mcp: McpProbeResult;
}

export async function probeHealth(config: ServiceConfigInput, timeoutMs = 5000): Promise<HealthProbeResult> {
  const resolved = ensureHttpServiceConfig(normalizeServiceConfig(config));
  const endpoints = buildServiceEndpoints(resolved);
  return probeHealthUrl(endpoints.health, timeoutMs);
}

export async function probeMcp(config: ServiceConfigInput, timeoutMs = 5000): Promise<McpProbeResult> {
  const resolved = ensureHttpServiceConfig(normalizeServiceConfig(config));
  const endpoints = buildServiceEndpoints(resolved);
  const client = new Client({
    name: "deep-obsidian-mcp-probe",
    version: "1.0.0",
  });
  const transport = new StreamableHTTPClientTransport(new URL(endpoints.mcp));

  const timeout = setTimeout(() => {
    void transport.close().catch(() => undefined);
    void client.close().catch(() => undefined);
  }, timeoutMs);

  try {
    await client.connect(transport);
    const tools = await client.listTools();
    const vaultInfo = await client.callTool({ name: "vault_info", arguments: {} });
    return {
      ok: true,
      toolCount: tools.tools.length,
      firstTool: tools.tools[0]?.name ?? null,
      vaultInfo,
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  } finally {
    clearTimeout(timeout);
    await transport.close().catch(() => undefined);
    await client.close().catch(() => undefined);
  }
}

export async function probeService(options: ProbeOptions): Promise<ProbeResult> {
  const resolved = ensureHttpServiceConfig(normalizeServiceConfig(options.config));
  const endpoints = buildServiceEndpoints(resolved);
  const [health, mcp] = await Promise.all([
    probeHealth(resolved, options.timeoutMs ?? 5000),
    probeMcp(resolved, options.timeoutMs ?? 5000),
  ]);

  return {
    endpoints,
    health,
    mcp,
  };
}
