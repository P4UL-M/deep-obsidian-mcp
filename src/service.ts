import os from "node:os";
import path from "node:path";

export type TransportMode = "stdio" | "http";
export type StdioMode = "auto" | "newline" | "framed";
export type EmbeddingProvider = "openai-compatible";

export interface ServiceHttpConfig {
  host: string;
  port: number;
  mcpPath: string;
  healthPath: string;
}

export interface ServiceAutoReindexConfig {
  enabled: boolean;
  debounceMs: number;
  intervalMs: number;
}

export interface ServiceEmbeddingConfig {
  provider?: EmbeddingProvider;
  model?: string;
  baseUrl?: string;
  apiKey?: string;
  apiKeyEnv?: string;
}

export interface ServiceHttpConfigInput {
  host?: string;
  port?: number | string;
  mcpPath?: string;
  healthPath?: string;
}

export interface ServiceAutoReindexConfigInput {
  enabled?: boolean;
  debounceMs?: number;
  intervalMs?: number;
}

export interface ServiceEmbeddingConfigInput {
  provider?: EmbeddingProvider;
  model?: string;
  baseUrl?: string;
  apiKey?: string;
  apiKeyEnv?: string;
}

export interface ServiceConfigInput {
  vaultPath?: string;
  indexDir?: string;
  transport?: TransportMode;
  stdioMode?: StdioMode;
  http?: ServiceHttpConfigInput;
  autoReindex?: ServiceAutoReindexConfigInput;
  embedding?: ServiceEmbeddingConfigInput;
  configFilePath?: string;
}

export interface ResolvedServiceConfig {
  vaultPath: string;
  indexDir: string;
  transport: TransportMode;
  stdioMode: StdioMode;
  http: ServiceHttpConfig;
  autoReindex: ServiceAutoReindexConfig;
  embedding: ServiceEmbeddingConfig;
  configFilePath?: string;
}

export interface PersistedServiceConfig {
  vaultPath: string;
  indexDir: string;
  transport: TransportMode;
  stdioMode: StdioMode;
  http: ServiceHttpConfig;
  autoReindex: ServiceAutoReindexConfig;
  embedding: Omit<ServiceEmbeddingConfig, "apiKey">;
}

export interface ServiceEndpoints {
  mcp: string;
  health: string;
}

export interface ServiceBootstrapContext {
  config: ResolvedServiceConfig;
  endpoints: ServiceEndpoints;
}

export function normalizeHttpPath(value: string | undefined, fallbackValue: string): string {
  const candidate = (value ?? fallbackValue).trim();
  if (!candidate || candidate === "/") {
    return "/";
  }
  return `/${candidate.replace(/^\/+/, "").replace(/\/+$/, "")}`;
}

export function getDefaultServiceConfigPath(homeDir = os.homedir()): string {
  return path.join(homeDir, ".config", "deep-obsidian-mcp", "config.json");
}

export function getDefaultIndexDir(vaultPath: string): string {
  return path.join(vaultPath, ".deep-obsidian-mcp");
}

export function buildServiceEndpoints(config: Pick<ResolvedServiceConfig, "http">): ServiceEndpoints {
  return {
    mcp: `http://${config.http.host}:${config.http.port}${config.http.mcpPath}`,
    health: `http://${config.http.host}:${config.http.port}${config.http.healthPath}`,
  };
}

function normalizePort(value: number | string | undefined, fallbackValue: number): number {
  const candidate = typeof value === "number" ? value : Number(value ?? fallbackValue);
  if (!Number.isInteger(candidate) || candidate < 1 || candidate > 65535) {
    throw new Error(`Invalid HTTP port: ${String(value ?? fallbackValue)}`);
  }
  return candidate;
}

function normalizeEmbeddingProvider(input: ServiceEmbeddingConfigInput): EmbeddingProvider | undefined {
  if (input.provider) {
    return input.provider;
  }
  if (input.model || input.baseUrl || input.apiKey || input.apiKeyEnv) {
    return "openai-compatible";
  }
  return undefined;
}

export function normalizeServiceConfig(input: ServiceConfigInput): ResolvedServiceConfig {
  const vaultPath = input.vaultPath?.trim();
  if (!vaultPath) {
    throw new Error("Missing vault path.");
  }

  const http = {
    host: input.http?.host?.trim() || "127.0.0.1",
    port: normalizePort(input.http?.port, 4100),
    mcpPath: normalizeHttpPath(input.http?.mcpPath, "/mcp"),
    healthPath: normalizeHttpPath(input.http?.healthPath, "/healthz"),
  };

  const autoReindex = {
    enabled: input.autoReindex?.enabled ?? true,
    debounceMs: Math.max(100, Math.trunc(input.autoReindex?.debounceMs ?? 1500)),
    intervalMs: Math.max(1000, Math.trunc(input.autoReindex?.intervalMs ?? 30000)),
  };

  const embedding = {
    provider: normalizeEmbeddingProvider(input.embedding ?? {}),
    model: input.embedding?.model?.trim() || undefined,
    baseUrl: input.embedding?.baseUrl?.trim() || undefined,
    apiKey: input.embedding?.apiKey?.trim() || undefined,
    apiKeyEnv: input.embedding?.apiKeyEnv?.trim() || undefined,
  };

  return {
    vaultPath,
    indexDir: input.indexDir?.trim() || getDefaultIndexDir(vaultPath),
    transport: input.transport ?? "http",
    stdioMode: input.stdioMode ?? "auto",
    http,
    autoReindex,
    embedding,
    configFilePath: input.configFilePath?.trim() || undefined,
  };
}

export function normalizePersistedServiceConfig(input: ServiceConfigInput): PersistedServiceConfig {
  const config = normalizeServiceConfig({
    vaultPath: input.vaultPath,
    indexDir: input.indexDir,
    transport: input.transport,
    stdioMode: input.stdioMode,
    http: input.http,
    autoReindex: input.autoReindex,
    embedding: input.embedding,
  });

  return toPersistedServiceConfig(config);
}

export function toPersistedServiceConfig(config: ResolvedServiceConfig): PersistedServiceConfig {
  const { apiKey: _apiKey, ...embedding } = config.embedding;
  return {
    vaultPath: config.vaultPath,
    indexDir: config.indexDir,
    transport: config.transport,
    stdioMode: config.stdioMode,
    http: {
      host: config.http.host,
      port: config.http.port,
      mcpPath: config.http.mcpPath,
      healthPath: config.http.healthPath,
    },
    autoReindex: {
      enabled: config.autoReindex.enabled,
      debounceMs: config.autoReindex.debounceMs,
      intervalMs: config.autoReindex.intervalMs,
    },
    embedding,
  };
}

export function ensureHttpServiceConfig(config: ResolvedServiceConfig): ResolvedServiceConfig {
  const normalized = normalizeServiceConfig(config);
  if (normalized.transport !== "http") {
    throw new Error(`HTTP service wrapper requires transport=http, received ${normalized.transport}`);
  }
  if (!normalized.http.host) {
    throw new Error("HTTP service wrapper requires a host.");
  }
  if (!normalized.http.port) {
    throw new Error("HTTP service wrapper requires a port.");
  }
  return normalized;
}

export function formatServiceConfig(config: PersistedServiceConfig | ResolvedServiceConfig): string {
  return JSON.stringify(
    "configFilePath" in config
      ? toPersistedServiceConfig(config)
      : config,
    null,
    2,
  );
}

export async function runHttpService<T>(
  config: ResolvedServiceConfig,
  bootstrap: (context: ServiceBootstrapContext) => Promise<T>,
): Promise<T> {
  const resolved = ensureHttpServiceConfig(config);
  return bootstrap({
    config: resolved,
    endpoints: buildServiceEndpoints(resolved),
  });
}
