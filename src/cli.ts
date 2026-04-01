import type { CliConfigOverrides, ParsedCommandLine, TopLevelCommand } from "./config.js";

const KNOWN_COMMANDS = new Set<TopLevelCommand>([
  "serve",
  "setup-service",
  "doctor",
  "print-config",
  "probe",
  "help",
  "version",
]);

function isKnownCommand(value: string | undefined): value is TopLevelCommand {
  return value !== undefined && KNOWN_COMMANDS.has(value as TopLevelCommand);
}

function parseBooleanLike(value: string | undefined, defaultValue = true): boolean {
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

function nextValue(tokens: string[], index: number, flagName: string): string {
  const next = tokens[index + 1];
  if (next === undefined || next.startsWith("-")) {
    throw new Error(`Missing value for ${flagName}.`);
  }
  return next;
}

function readOptionalBooleanValue(token: string, tokens: string[], index: number): { value: boolean; advanceBy: number } {
  if (token.includes("=")) {
    const value = token.slice(token.indexOf("=") + 1);
    return { value: parseBooleanLike(value, true), advanceBy: 1 };
  }

  const next = tokens[index + 1];
  if (next !== undefined && !next.startsWith("-")) {
    return { value: parseBooleanLike(next, true), advanceBy: 2 };
  }

  return { value: true, advanceBy: 1 };
}

function readFlagValue(
  token: string,
  tokens: string[],
  index: number,
  flagName: string,
): { value: string | undefined; advanceBy: number } {
  const equalsIndex = token.indexOf("=");
  if (equalsIndex >= 0) {
    return {
      value: token.slice(equalsIndex + 1),
      advanceBy: 1,
    };
  }

  if (index + 1 >= tokens.length || tokens[index + 1].startsWith("-")) {
    throw new Error(`Missing value for ${flagName}.`);
  }

  return {
    value: nextValue(tokens, index, flagName),
    advanceBy: 2,
  };
}

function parseInteger(token: string, flagName: string): number {
  const parsed = Number.parseInt(token, 10);
  if (!Number.isFinite(parsed)) {
    throw new Error(`Invalid value for ${flagName}: ${token}`);
  }
  return parsed;
}

function assignHttp(
  target: CliConfigOverrides,
  key: "host" | "port" | "mcpPath" | "healthPath",
  value: string | number,
): void {
  target.http ??= {};
  target.http[key] = value as never;
}

function assignAutoReindex(
  target: CliConfigOverrides,
  key: "enabled" | "debounceMs" | "intervalMs",
  value: boolean | number,
): void {
  target.autoReindex ??= {};
  target.autoReindex[key] = value as never;
}

function assignEmbedding(
  target: CliConfigOverrides,
  key: "provider" | "model" | "baseUrl" | "apiKey" | "apiKeyEnv",
  value: string | undefined,
): void {
  target.embedding ??= {};
  if (value !== undefined) {
    target.embedding[key] = value as never;
  }
}

function parseTokens(tokens: string[]): ParsedCommandLine {
  const flags: CliConfigOverrides = {};
  const positionals: string[] = [];
  let configPath: string | undefined;
  let dryRun = false;
  let json = false;

  for (let index = 0; index < tokens.length;) {
    const token = tokens[index];
    if (!token.startsWith("-")) {
      positionals.push(token);
      index += 1;
      continue;
    }

    if (token === "--help" || token === "-h") {
      index += 1;
      continue;
    }
    if (token === "--version" || token === "-v") {
      index += 1;
      continue;
    }
    if (token === "--dry-run") {
      const parsed = readOptionalBooleanValue(token, tokens, index);
      dryRun = parsed.value;
      index += parsed.advanceBy;
      continue;
    }
    if (token === "--no-dry-run") {
      dryRun = false;
      index += 1;
      continue;
    }
    if (token === "--json") {
      const parsed = readOptionalBooleanValue(token, tokens, index);
      json = parsed.value;
      index += parsed.advanceBy;
      continue;
    }
    if (token === "--no-json") {
      json = false;
      index += 1;
      continue;
    }

    const flagName = token.includes("=") ? token.slice(0, token.indexOf("=")) : token;

    switch (flagName) {
      case "--config": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          configPath = value;
        }
        index += advanceBy;
        break;
      }
      case "--vault":
      case "--vault-path": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          flags.vaultPath = value;
        }
        index += advanceBy;
        break;
      }
      case "--index-dir": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          flags.indexDir = value;
        }
        index += advanceBy;
        break;
      }
      case "--transport": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value === "http" || value === "stdio") {
          flags.transport = value;
        }
        index += advanceBy;
        break;
      }
      case "--stdio-mode": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value === "auto" || value === "newline" || value === "framed") {
          flags.stdioMode = value;
        }
        index += advanceBy;
        break;
      }
      case "--host": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignHttp(flags, "host", value);
        }
        index += advanceBy;
        break;
      }
      case "--port": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignHttp(flags, "port", parseInteger(value, flagName));
        }
        index += advanceBy;
        break;
      }
      case "--mcp-path": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignHttp(flags, "mcpPath", value);
        }
        index += advanceBy;
        break;
      }
      case "--health-path": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignHttp(flags, "healthPath", value);
        }
        index += advanceBy;
        break;
      }
      case "--auto-reindex": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        assignAutoReindex(flags, "enabled", value === undefined ? true : parseBooleanLike(value));
        index += advanceBy;
        break;
      }
      case "--no-auto-reindex": {
        assignAutoReindex(flags, "enabled", false);
        index += 1;
        break;
      }
      case "--reindex-debounce-ms": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignAutoReindex(flags, "debounceMs", parseInteger(value, flagName));
        }
        index += advanceBy;
        break;
      }
      case "--reindex-interval-ms": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value !== undefined) {
          assignAutoReindex(flags, "intervalMs", parseInteger(value, flagName));
        }
        index += advanceBy;
        break;
      }
      case "--embedding-provider": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        if (value === "openai-compatible") {
          assignEmbedding(flags, "provider", value);
        }
        index += advanceBy;
        break;
      }
      case "--embedding-model": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        assignEmbedding(flags, "model", value);
        index += advanceBy;
        break;
      }
      case "--embedding-base-url": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        assignEmbedding(flags, "baseUrl", value);
        index += advanceBy;
        break;
      }
      case "--embedding-api-key": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        assignEmbedding(flags, "apiKey", value);
        index += advanceBy;
        break;
      }
      case "--embedding-api-key-env": {
        const { value, advanceBy } = readFlagValue(token, tokens, index, flagName);
        assignEmbedding(flags, "apiKeyEnv", value);
        index += advanceBy;
        break;
      }
      default: {
        index += 1;
        break;
      }
    }
  }

  const command: TopLevelCommand = positionals[0] && isKnownCommand(positionals[0]) ? (positionals.shift()! as TopLevelCommand) : "serve";

  return {
    command,
    positionals,
    flags,
    configPath,
    dryRun,
    json,
  };
}

export function parseCli(argv: readonly string[]): ParsedCommandLine {
  const tokens = [...argv];
  if (tokens.length > 0) {
    const first = tokens[0];
    if (isKnownCommand(first)) {
      return parseTokens(tokens);
    }
    if (first === "--help" || first === "-h") {
      return { command: "help", positionals: [], flags: {}, dryRun: false, json: false };
    }
    if (first === "--version" || first === "-v") {
      return { command: "version", positionals: [], flags: {}, dryRun: false, json: false };
    }
  }

  return parseTokens(tokens);
}
