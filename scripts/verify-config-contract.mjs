#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

const ROOT_DIR = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_FIXTURE = path.join(ROOT_DIR, "tests", "fixtures", "behavior", "config-resolution-cases.json");

function normalizeHttpPath(value, fallbackValue) {
  const candidate = String(value ?? fallbackValue ?? "").trim();
  if (!candidate || candidate === "/") {
    return "/";
  }
  return `/${candidate.replace(/^\/+/, "").replace(/\/+$/, "")}`;
}

function firstDefined(...values) {
  for (const value of values) {
    if (value !== undefined && value !== null && value !== "") {
      return value;
    }
  }
  return undefined;
}

function parseBoolean(value, fallback) {
  if (value === undefined || value === null || value === "") {
    return fallback;
  }
  const normalized = String(value).trim().toLowerCase();
  if (["1", "true", "yes", "on"].includes(normalized)) {
    return true;
  }
  if (["0", "false", "no", "off"].includes(normalized)) {
    return false;
  }
  return fallback;
}

function parseNumber(value, fallback) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function resolveConfig({ defaults, env, config, cli }) {
  const resolved = {
    vaultPath: firstDefined(cli?.vaultPath, config?.vaultPath, env?.DEEP_OBSIDIAN_VAULT_PATH, env?.OBSIDIAN_VAULT_PATH, defaults.vaultPath),
    indexDir: firstDefined(cli?.indexDir, config?.indexDir, env?.DEEP_OBSIDIAN_INDEX_DIR, defaults.indexDir),
    transport: firstDefined(cli?.transport, config?.transport, env?.DEEP_OBSIDIAN_TRANSPORT_MODE, env?.MCP_TRANSPORT_MODE, defaults.transport),
    stdioMode: firstDefined(cli?.stdioMode, config?.stdioMode, env?.DEEP_OBSIDIAN_STDIO_MODE, env?.MCP_STDIO_MODE, defaults.stdioMode),
    http: {
      host: firstDefined(cli?.http?.host, config?.http?.host, env?.DEEP_OBSIDIAN_HTTP_HOST, env?.MCP_HTTP_HOST, defaults.http.host),
      port: parseNumber(firstDefined(cli?.http?.port, config?.http?.port, env?.DEEP_OBSIDIAN_HTTP_PORT, env?.MCP_HTTP_PORT, defaults.http.port), defaults.http.port),
      mcpPath: normalizeHttpPath(firstDefined(cli?.http?.mcpPath, config?.http?.mcpPath, env?.DEEP_OBSIDIAN_HTTP_PATH, env?.MCP_HTTP_PATH, defaults.http.mcpPath), defaults.http.mcpPath),
      healthPath: normalizeHttpPath(firstDefined(cli?.http?.healthPath, config?.http?.healthPath, env?.DEEP_OBSIDIAN_HTTP_HEALTH_PATH, env?.MCP_HTTP_HEALTH_PATH, defaults.http.healthPath), defaults.http.healthPath),
    },
    autoReindex: {
      enabled: parseBoolean(firstDefined(cli?.autoReindex?.enabled, config?.autoReindex?.enabled, env?.DEEP_OBSIDIAN_AUTO_REINDEX, env?.AUTO_REINDEX), defaults.autoReindex.enabled),
      debounceMs: parseNumber(firstDefined(cli?.autoReindex?.debounceMs, config?.autoReindex?.debounceMs, env?.DEEP_OBSIDIAN_REINDEX_DEBOUNCE_MS, env?.REINDEX_DEBOUNCE_MS), defaults.autoReindex.debounceMs),
      intervalMs: parseNumber(firstDefined(cli?.autoReindex?.intervalMs, config?.autoReindex?.intervalMs, env?.DEEP_OBSIDIAN_REINDEX_INTERVAL_MS, env?.REINDEX_INTERVAL_MS), defaults.autoReindex.intervalMs),
    },
    embedding: {
      provider: firstDefined(cli?.embedding?.provider, config?.embedding?.provider, env?.DEEP_OBSIDIAN_EMBEDDING_PROVIDER, env?.EMBEDDING_PROVIDER),
      model: firstDefined(cli?.embedding?.model, config?.embedding?.model, env?.DEEP_OBSIDIAN_EMBEDDING_MODEL, env?.EMBEDDING_MODEL, env?.OPENAI_EMBEDDING_MODEL),
      baseUrl: firstDefined(cli?.embedding?.baseUrl, config?.embedding?.baseUrl, env?.DEEP_OBSIDIAN_EMBEDDING_BASE_URL, env?.EMBEDDING_BASE_URL, env?.OPENAI_BASE_URL),
      apiKeyEnv: firstDefined(cli?.embedding?.apiKeyEnv, config?.embedding?.apiKeyEnv, env?.DEEP_OBSIDIAN_EMBEDDING_API_KEY_ENV, env?.EMBEDDING_API_KEY_ENV, env?.OPENAI_API_KEY ? "OPENAI_API_KEY" : undefined),
    },
  };

  if (!resolved.embedding.provider && resolved.embedding.model) {
    resolved.embedding.provider = "openai-compatible";
  }
  return resolved;
}

async function main() {
  const fixturePath = process.argv[2] ?? DEFAULT_FIXTURE;
  const payload = JSON.parse(await readFile(fixturePath, "utf8"));
  assert.ok(Array.isArray(payload.cases), "fixture must provide a cases array");

  for (const testCase of payload.cases) {
    const actual = resolveConfig(testCase);
    assert.deepEqual(actual, testCase.expected, `config resolution mismatch for case: ${testCase.name}`);
  }

  console.log(JSON.stringify({ fixturePath, cases: payload.cases.length, status: "ok" }, null, 2));
}

main().catch((error) => {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  process.exit(1);
});

export { resolveConfig };
