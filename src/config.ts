import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";

import type { EmbeddingConfig } from "./embeddings.js";
import type { StdioMode } from "./transport.js";

export type TransportMode = "stdio" | "http";
export type TopLevelCommand = "serve" | "setup-service" | "doctor" | "print-config" | "probe" | "help" | "version";
export type ConfigSource = "cli" | "config" | "env" | "default";

export interface HttpConfig {
  host: string;
  port: number;
  mcpPath: string;
  healthPath: string;
}

export interface AutoReindexConfig {
  enabled: boolean;
  debounceMs: number;
  intervalMs: number;
}

export interface ConfigEmbeddingConfig {
  provider?: EmbeddingConfig["provider"];
  model?: string;
  baseUrl?: string;
  apiKey?: string;
  apiKeyEnv?: string;
}

export interface PersistedConfigFile {
  vaultPath?: string;
  indexDir?: string;
  transport?: TransportMode;
  stdioMode?: StdioMode;
  http?: Partial<HttpConfig>;
  autoReindex?: Partial<AutoReindexConfig>;
  embedding?: ConfigEmbeddingConfig;
}

export interface CliConfigOverrides {
  vaultPath?: string;
  indexDir?: string;
  transport?: TransportMode;
  stdioMode?: StdioMode;
  http?: Partial<HttpConfig>;
  autoReindex?: Partial<AutoReindexConfig>;
  embedding?: ConfigEmbeddingConfig;
}

export interface ParsedCommandLine {
  command: TopLevelCommand;
  positionals: string[];
  flags: CliConfigOverrides;
  configPath?: string;
  dryRun: boolean;
  json: boolean;
}

export interface ResolvedConfigSources {
  vaultPath: ConfigSource;
  indexDir: ConfigSource;
  transport: ConfigSource;
  stdioMode: ConfigSource;
  httpHost: ConfigSource;
  httpPort: ConfigSource;
  httpMcpPath: ConfigSource;
  httpHealthPath: ConfigSource;
  autoReindexEnabled: ConfigSource;
  autoReindexDebounceMs: ConfigSource;
  autoReindexIntervalMs: ConfigSource;
  embeddingProvider: ConfigSource;
  embeddingModel: ConfigSource;
  embeddingBaseUrl: ConfigSource;
  embeddingApiKey: ConfigSource;
}

export interface ResolvedRuntimeConfig {
  vaultPath?: string;
  indexDir?: string;
  transport: TransportMode;
  stdioMode: StdioMode;
  http: HttpConfig;
  autoReindex: AutoReindexConfig;
  embedding: ConfigEmbeddingConfig;
  configFilePath: string;
  configFile: PersistedConfigFile | null;
  sources: ResolvedConfigSources;
}

export interface EntryPointRuntimeConfig {
  vaultPath: string;
  indexDir?: string;
  transportMode: TransportMode;
  stdioMode: StdioMode;
  httpHost: string;
  httpPort: number;
  httpMcpPath: string;
  httpHealthPath: string;
  embeddingConfig: EmbeddingConfig;
  autoReindex: boolean;
  reindexDebounceMs: number;
  reindexIntervalMs: number;
}

export const DEFAULT_HTTP_HOST = "127.0.0.1";
export const DEFAULT_HTTP_PORT = 4100;
export const DEFAULT_HTTP_MCP_PATH = "/mcp";
export const DEFAULT_HTTP_HEALTH_PATH = "/healthz";
export const DEFAULT_AUTO_REINDEX_DEBOUNCE_MS = 1500;
export const DEFAULT_AUTO_REINDEX_INTERVAL_MS = 30000;
export const DEFAULT_CONFIG_DIR_NAME = ".config";
export const DEFAULT_CONFIG_APP_DIR = "deep-obsidian-mcp";
export const DEFAULT_CONFIG_FILE_NAME = "config.json";

function parseBoolean(value: string | undefined, defaultValue: boolean): boolean {
  if (value === undefined) {
    return defaultValue;
  }

  const normalized = value.trim().toLowerCase();
  if (["1", "true", "yes", "on"].includes(normalized)) {
    return true;
  }
  if (["0", "false", "no", "off"].includes(normalized)) {
    return false;
  }
  return defaultValue;
}

function parseInteger(value: string | undefined, defaultValue: number): number {
  if (value === undefined) {
    return defaultValue;
  }
  const parsed = Number.parseInt(value, 10);
  return Number.isFinite(parsed) ? parsed : defaultValue;
}

export function normalizeHttpPath(value: string | undefined, fallbackValue: string): string {
  const candidate = (value ?? fallbackValue).trim();
  if (!candidate || candidate === "/") {
    return "/";
  }
  return `/${candidate.replace(/^\/+/, "").replace(/\/+$/, "")}`;
}

export function expandHome(value: string | undefined): string | undefined {
  if (!value) {
    return value;
  }

  if (value === "~") {
    return os.homedir();
  }

  if (value.startsWith("~/")) {
    return path.join(os.homedir(), value.slice(2));
  }

  if (value.startsWith("~\\")) {
    return path.join(os.homedir(), value.slice(2));
  }

  return value;
}

export function resolveDefaultConfigDir(cwd = process.cwd()): string {
  const configHome = process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), DEFAULT_CONFIG_DIR_NAME);
  return path.resolve(expandHome(configHome) ?? path.join(cwd, DEFAULT_CONFIG_DIR_NAME));
}

export function defaultConfigPath(cwd = process.cwd()): string {
  return path.join(resolveDefaultConfigDir(cwd), DEFAULT_CONFIG_APP_DIR, DEFAULT_CONFIG_FILE_NAME);
}

export function normalizePersistedConfig(value: PersistedConfigFile): PersistedConfigFile {
  const normalized: PersistedConfigFile = {};

  if (value.vaultPath) {
    normalized.vaultPath = expandHome(value.vaultPath);
  }
  if (value.indexDir) {
    normalized.indexDir = expandHome(value.indexDir);
  }
  if (value.transport === "stdio" || value.transport === "http") {
    normalized.transport = value.transport;
  }
  if (value.stdioMode === "auto" || value.stdioMode === "newline" || value.stdioMode === "framed") {
    normalized.stdioMode = value.stdioMode;
  }

  if (value.http) {
    const http: Partial<HttpConfig> = {};
    if (value.http.host) {
      http.host = value.http.host.trim();
    }
    if (value.http.port !== undefined) {
      http.port = normalizePort(value.http.port, DEFAULT_HTTP_PORT);
    }
    if (value.http.mcpPath) {
      http.mcpPath = normalizeHttpPath(value.http.mcpPath, DEFAULT_HTTP_MCP_PATH);
    }
    if (value.http.healthPath) {
      http.healthPath = normalizeHttpPath(value.http.healthPath, DEFAULT_HTTP_HEALTH_PATH);
    }
    if (Object.keys(http).length > 0) {
      normalized.http = http;
    }
  }

  if (value.autoReindex) {
    const autoReindex: Partial<AutoReindexConfig> = {};
    if (value.autoReindex.enabled !== undefined) {
      autoReindex.enabled = value.autoReindex.enabled;
    }
    if (value.autoReindex.debounceMs !== undefined) {
      autoReindex.debounceMs = normalizePositiveInteger(value.autoReindex.debounceMs, DEFAULT_AUTO_REINDEX_DEBOUNCE_MS);
    }
    if (value.autoReindex.intervalMs !== undefined) {
      autoReindex.intervalMs = normalizePositiveInteger(value.autoReindex.intervalMs, DEFAULT_AUTO_REINDEX_INTERVAL_MS);
    }
    if (Object.keys(autoReindex).length > 0) {
      normalized.autoReindex = autoReindex;
    }
  }

  if (value.embedding) {
    const embedding = normalizeConfigEmbedding(value.embedding);
    if (Object.keys(embedding).length > 0) {
      normalized.embedding = embedding;
    }
  }

  return normalized;
}

export function normalizeHttpConfig(value?: Partial<HttpConfig>): HttpConfig {
  return {
    host: value?.host?.trim() || DEFAULT_HTTP_HOST,
    port: normalizePort(value?.port, DEFAULT_HTTP_PORT),
    mcpPath: normalizeHttpPath(value?.mcpPath, DEFAULT_HTTP_MCP_PATH),
    healthPath: normalizeHttpPath(value?.healthPath, DEFAULT_HTTP_HEALTH_PATH),
  };
}

export function normalizeAutoReindexConfig(value?: Partial<AutoReindexConfig>): AutoReindexConfig {
  return {
    enabled: value?.enabled ?? true,
    debounceMs: normalizePositiveInteger(value?.debounceMs, DEFAULT_AUTO_REINDEX_DEBOUNCE_MS),
    intervalMs: normalizePositiveInteger(value?.intervalMs, DEFAULT_AUTO_REINDEX_INTERVAL_MS),
  };
}

export function normalizeConfigEmbedding(value?: ConfigEmbeddingConfig): ConfigEmbeddingConfig {
  if (!value) {
    return {};
  }

  const normalized: ConfigEmbeddingConfig = {};
  if (value.provider === "openai-compatible") {
    normalized.provider = value.provider;
  }
  if (value.model) {
    normalized.model = value.model.trim();
  }
  if (value.baseUrl) {
    normalized.baseUrl = value.baseUrl.trim();
  }
  if (value.apiKey) {
    normalized.apiKey = value.apiKey;
  }
  if (value.apiKeyEnv) {
    normalized.apiKeyEnv = value.apiKeyEnv.trim();
  }
  return normalized;
}

export function normalizePort(value: number | string | undefined, defaultValue: number): number {
  if (value === undefined || value === null) {
    return defaultValue;
  }
  const parsed = typeof value === "number" ? value : Number.parseInt(value, 10);
  return Number.isInteger(parsed) && parsed > 0 && parsed <= 65535 ? parsed : defaultValue;
}

export function normalizePositiveInteger(value: number | string | undefined, defaultValue: number): number {
  if (value === undefined || value === null) {
    return defaultValue;
  }
  const parsed = typeof value === "number" ? value : Number.parseInt(value, 10);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : defaultValue;
}

export function resolveConfigFilePath(configPath?: string, cwd = process.cwd()): string {
  return path.resolve(expandHome(configPath) ?? defaultConfigPath(cwd));
}

export async function readConfigFile(configPath: string): Promise<PersistedConfigFile | null> {
  const resolved = path.resolve(expandHome(configPath) ?? configPath);
  const raw = await fs.readFile(resolved, "utf8").catch(() => null);
  if (raw === null) {
    return null;
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new Error(`Failed to parse config file ${resolved}: ${error instanceof Error ? error.message : String(error)}`);
  }

  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error(`Config file must contain a JSON object: ${resolved}`);
  }

  return normalizePersistedConfig(parsed as PersistedConfigFile);
}

export async function writeConfigFile(configPath: string, config: PersistedConfigFile): Promise<void> {
  const resolved = path.resolve(expandHome(configPath) ?? configPath);
  await fs.mkdir(path.dirname(resolved), { recursive: true });
  const payload = `${JSON.stringify(normalizePersistedConfig(config), null, 2)}\n`;
  await fs.writeFile(resolved, payload, "utf8");
}

function pickSource<T>(...values: Array<{ value: T | undefined; source: ConfigSource }>): { value: T | undefined; source: ConfigSource } {
  for (const candidate of values) {
    if (candidate.value !== undefined) {
      return candidate;
    }
  }
  return { value: undefined, source: "default" };
}

function envValue(env: NodeJS.ProcessEnv, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = env[key];
    if (value !== undefined && value !== "") {
      return value;
    }
  }
  return undefined;
}

function resolveEmbeddingApiKey(
  env: NodeJS.ProcessEnv,
  configEmbedding: ConfigEmbeddingConfig | undefined,
  cliEmbedding: ConfigEmbeddingConfig | undefined,
): { value: string | undefined; source: ConfigSource } {
  const explicitApiKey = cliEmbedding?.apiKey ?? configEmbedding?.apiKey;
  if (explicitApiKey !== undefined) {
    return { value: explicitApiKey, source: cliEmbedding?.apiKey !== undefined ? "cli" : "config" };
  }

  const envName = cliEmbedding?.apiKeyEnv ?? configEmbedding?.apiKeyEnv;
  if (envName) {
    const resolved = env[envName];
    if (resolved !== undefined && resolved !== "") {
      return { value: resolved, source: cliEmbedding?.apiKeyEnv !== undefined ? "cli" : "config" };
    }
  }

  const envValueFromCommonNames = envValue(env, [
    "DEEP_OBSIDIAN_EMBEDDING_API_KEY",
    "EMBEDDING_API_KEY",
    "OPENAI_API_KEY",
  ]);
  if (envValueFromCommonNames !== undefined) {
    return { value: envValueFromCommonNames, source: "env" };
  }

  return { value: undefined, source: "default" };
}

function resolveEmbeddingBaseUrl(
  env: NodeJS.ProcessEnv,
  configEmbedding: ConfigEmbeddingConfig | undefined,
  cliEmbedding: ConfigEmbeddingConfig | undefined,
): { value: string | undefined; source: ConfigSource } {
  return pickSource<string>(
    { value: cliEmbedding?.baseUrl, source: "cli" },
    { value: configEmbedding?.baseUrl, source: "config" },
    {
      value: envValue(env, [
        "DEEP_OBSIDIAN_EMBEDDING_BASE_URL",
        "EMBEDDING_BASE_URL",
        "OPENAI_BASE_URL",
      ]),
      source: "env",
    },
  );
}

function resolveEmbeddingModel(
  env: NodeJS.ProcessEnv,
  configEmbedding: ConfigEmbeddingConfig | undefined,
  cliEmbedding: ConfigEmbeddingConfig | undefined,
): { value: string | undefined; source: ConfigSource } {
  return pickSource<string>(
    { value: cliEmbedding?.model, source: "cli" },
    { value: configEmbedding?.model, source: "config" },
    {
      value: envValue(env, [
        "DEEP_OBSIDIAN_EMBEDDING_MODEL",
        "EMBEDDING_MODEL",
        "OPENAI_EMBEDDING_MODEL",
      ]),
      source: "env",
    },
  );
}

function resolveEmbeddingProvider(
  env: NodeJS.ProcessEnv,
  configEmbedding: ConfigEmbeddingConfig | undefined,
  cliEmbedding: ConfigEmbeddingConfig | undefined,
  modelValue: string | undefined,
): { value: EmbeddingConfig["provider"] | undefined; source: ConfigSource } {
  const explicitValues: Array<{ value: string | undefined; source: ConfigSource }> = [
    { value: cliEmbedding?.provider, source: "cli" },
    { value: configEmbedding?.provider, source: "config" },
    { value: envValue(env, ["DEEP_OBSIDIAN_EMBEDDING_PROVIDER", "EMBEDDING_PROVIDER"]), source: "env" },
  ];

  for (const candidate of explicitValues) {
    if (candidate.value === "openai-compatible") {
      return { value: candidate.value, source: candidate.source };
    }
  }

  if (modelValue) {
    return { value: "openai-compatible", source: "default" };
  }

  return { value: undefined, source: "default" };
}

function resolveVaultPath(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: ParsedCommandLine,
): { value: string | undefined; source: ConfigSource } {
  return pickSource<string>(
    { value: cli.flags.vaultPath ?? cli.positionals[0], source: "cli" },
    { value: configFile?.vaultPath, source: "config" },
    {
      value: envValue(env, ["DEEP_OBSIDIAN_VAULT_PATH", "OBSIDIAN_VAULT_PATH"]),
      source: "env",
    },
  );
}

function resolveIndexDir(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: CliConfigOverrides,
): { value: string | undefined; source: ConfigSource } {
  return pickSource<string>(
    { value: cli.indexDir, source: "cli" },
    { value: configFile?.indexDir, source: "config" },
    {
      value: envValue(env, ["DEEP_OBSIDIAN_INDEX_DIR", "INDEX_DIR"]),
      source: "env",
    },
  );
}

function resolveTransport(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: CliConfigOverrides,
): { value: TransportMode; source: ConfigSource } {
  const resolved = pickSource<TransportMode>(
    { value: cli.transport, source: "cli" },
    { value: configFile?.transport, source: "config" },
    {
      value: envValue(env, ["MCP_TRANSPORT_MODE", "DEEP_OBSIDIAN_TRANSPORT_MODE"]) as TransportMode | undefined,
      source: "env",
    },
  );
  return {
    value: resolved.value === "http" ? "http" : "stdio",
    source: resolved.source,
  };
}

function resolveStdioMode(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: CliConfigOverrides,
): { value: StdioMode; source: ConfigSource } {
  const resolved = pickSource<StdioMode>(
    { value: cli.stdioMode, source: "cli" },
    { value: configFile?.stdioMode, source: "config" },
    {
      value: envValue(env, ["MCP_STDIO_MODE", "DEEP_OBSIDIAN_STDIO_MODE"]) as StdioMode | undefined,
      source: "env",
    },
  );
  return {
    value: resolved.value === "newline" || resolved.value === "framed" ? resolved.value : "auto",
    source: resolved.source,
  };
}

function resolveHttpConfig(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: CliConfigOverrides,
): { value: HttpConfig; sources: Pick<ResolvedConfigSources, "httpHost" | "httpPort" | "httpMcpPath" | "httpHealthPath"> } {
  const host = pickSource<string>(
    { value: cli.http?.host, source: "cli" },
    { value: configFile?.http?.host, source: "config" },
    {
      value: envValue(env, ["MCP_HTTP_HOST", "DEEP_OBSIDIAN_HOST", "DEEP_OBSIDIAN_HTTP_HOST"]),
      source: "env",
    },
  );
  const port = pickSource<number | string>(
    { value: cli.http?.port, source: "cli" },
    { value: configFile?.http?.port, source: "config" },
    {
      value: envValue(env, ["MCP_HTTP_PORT", "DEEP_OBSIDIAN_PORT", "DEEP_OBSIDIAN_HTTP_PORT"]),
      source: "env",
    },
  );
  const mcpPath = pickSource<string>(
    { value: cli.http?.mcpPath, source: "cli" },
    { value: configFile?.http?.mcpPath, source: "config" },
    {
      value: envValue(env, ["MCP_HTTP_PATH", "DEEP_OBSIDIAN_MCP_PATH"]),
      source: "env",
    },
  );
  const healthPath = pickSource<string>(
    { value: cli.http?.healthPath, source: "cli" },
    { value: configFile?.http?.healthPath, source: "config" },
    {
      value: envValue(env, ["MCP_HTTP_HEALTH_PATH", "DEEP_OBSIDIAN_HEALTH_PATH"]),
      source: "env",
    },
  );

  return {
    value: {
      host: host.value?.trim() || DEFAULT_HTTP_HOST,
      port: normalizePort(port.value, DEFAULT_HTTP_PORT),
      mcpPath: normalizeHttpPath(mcpPath.value, DEFAULT_HTTP_MCP_PATH),
      healthPath: normalizeHttpPath(healthPath.value, DEFAULT_HTTP_HEALTH_PATH),
    },
    sources: {
      httpHost: host.source,
      httpPort: port.source,
      httpMcpPath: mcpPath.source,
      httpHealthPath: healthPath.source,
    },
  };
}

function resolveAutoReindexConfig(
  env: NodeJS.ProcessEnv,
  configFile: PersistedConfigFile | null,
  cli: CliConfigOverrides,
): { value: AutoReindexConfig; sources: Pick<ResolvedConfigSources, "autoReindexEnabled" | "autoReindexDebounceMs" | "autoReindexIntervalMs"> } {
  const enabled = pickSource<boolean>(
    { value: cli.autoReindex?.enabled, source: "cli" },
    { value: configFile?.autoReindex?.enabled, source: "config" },
    {
      value: parseBoolean(envValue(env, ["AUTO_REINDEX", "DEEP_OBSIDIAN_AUTO_REINDEX"]), true),
      source: "env",
    },
  );
  const debounceMs = pickSource<number | string>(
    { value: cli.autoReindex?.debounceMs, source: "cli" },
    { value: configFile?.autoReindex?.debounceMs, source: "config" },
    {
      value: envValue(env, ["REINDEX_DEBOUNCE_MS", "DEEP_OBSIDIAN_REINDEX_DEBOUNCE_MS"]),
      source: "env",
    },
  );
  const intervalMs = pickSource<number | string>(
    { value: cli.autoReindex?.intervalMs, source: "cli" },
    { value: configFile?.autoReindex?.intervalMs, source: "config" },
    {
      value: envValue(env, ["REINDEX_INTERVAL_MS", "DEEP_OBSIDIAN_REINDEX_INTERVAL_MS"]),
      source: "env",
    },
  );

  return {
    value: {
      enabled: enabled.value ?? true,
      debounceMs: normalizePositiveInteger(debounceMs.value, DEFAULT_AUTO_REINDEX_DEBOUNCE_MS),
      intervalMs: normalizePositiveInteger(intervalMs.value, DEFAULT_AUTO_REINDEX_INTERVAL_MS),
    },
    sources: {
      autoReindexEnabled: enabled.source,
      autoReindexDebounceMs: debounceMs.source,
      autoReindexIntervalMs: intervalMs.source,
    },
  };
}

export function toEmbeddingConfig(resolved: ResolvedRuntimeConfig): EmbeddingConfig {
  const normalized = normalizeConfigEmbedding(resolved.embedding);
  const backend = normalized.provider && normalized.model ? "embedding" : "sparse";
  return {
    backend,
    provider: normalized.provider,
    model: normalized.model,
    baseUrl: normalized.baseUrl,
    apiKey: normalized.apiKey,
  };
}

export function toEntryPointRuntimeConfig(resolved: ResolvedRuntimeConfig): EntryPointRuntimeConfig {
  return {
    vaultPath: resolved.vaultPath ?? "",
    indexDir: resolved.indexDir,
    transportMode: resolved.transport,
    stdioMode: resolved.stdioMode,
    httpHost: resolved.http.host,
    httpPort: resolved.http.port,
    httpMcpPath: resolved.http.mcpPath,
    httpHealthPath: resolved.http.healthPath,
    embeddingConfig: toEmbeddingConfig(resolved),
    autoReindex: resolved.autoReindex.enabled,
    reindexDebounceMs: resolved.autoReindex.debounceMs,
    reindexIntervalMs: resolved.autoReindex.intervalMs,
  };
}

export async function resolveRuntimeConfig(
  cli: ParsedCommandLine,
  options?: { env?: NodeJS.ProcessEnv; cwd?: string },
): Promise<ResolvedRuntimeConfig> {
  const env = options?.env ?? process.env;
  const cwd = options?.cwd ?? process.cwd();
  const configPath = resolveConfigFilePath(cli.configPath, cwd);
  const configFile = await readConfigFile(configPath);
  const cliFlags = cli.flags;

  const vaultPath = resolveVaultPath(env, configFile, cli);
  const indexDir = resolveIndexDir(env, configFile, cliFlags);
  const transport = resolveTransport(env, configFile, cliFlags);
  const stdioMode = resolveStdioMode(env, configFile, cliFlags);
  const httpConfig = resolveHttpConfig(env, configFile, cliFlags);
  const autoReindexConfig = resolveAutoReindexConfig(env, configFile, cliFlags);

  const cliEmbedding = cliFlags.embedding;
  const configEmbedding = configFile?.embedding;
  const model = resolveEmbeddingModel(env, configEmbedding, cliEmbedding);
  const provider = resolveEmbeddingProvider(env, configEmbedding, cliEmbedding, model.value);
  const baseUrl = resolveEmbeddingBaseUrl(env, configEmbedding, cliEmbedding);
  const apiKey = resolveEmbeddingApiKey(env, configEmbedding, cliEmbedding);

  return {
    vaultPath: expandHome(vaultPath.value),
    indexDir: expandHome(indexDir.value),
    transport: transport.value,
    stdioMode: stdioMode.value,
    http: httpConfig.value,
    autoReindex: autoReindexConfig.value,
    embedding: normalizeConfigEmbedding({
      provider: provider.value,
      model: model.value,
      baseUrl: baseUrl.value,
      apiKey: apiKey.value,
      apiKeyEnv: cliEmbedding?.apiKeyEnv ?? configEmbedding?.apiKeyEnv,
    }),
    configFilePath: configPath,
    configFile,
    sources: {
      vaultPath: vaultPath.source,
      indexDir: indexDir.source,
      transport: transport.source,
      stdioMode: stdioMode.source,
      httpHost: httpConfig.sources.httpHost,
      httpPort: httpConfig.sources.httpPort,
      httpMcpPath: httpConfig.sources.httpMcpPath,
      httpHealthPath: httpConfig.sources.httpHealthPath,
      autoReindexEnabled: autoReindexConfig.sources.autoReindexEnabled,
      autoReindexDebounceMs: autoReindexConfig.sources.autoReindexDebounceMs,
      autoReindexIntervalMs: autoReindexConfig.sources.autoReindexIntervalMs,
      embeddingProvider: provider.source,
      embeddingModel: model.source,
      embeddingBaseUrl: baseUrl.source,
      embeddingApiKey: apiKey.source,
    },
  };
}
