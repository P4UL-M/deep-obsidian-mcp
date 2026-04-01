#!/usr/bin/env node

import { createServer as createHttpServer } from "node:http";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";

import { parseCli } from "./cli.js";
import { printConfig, probeService, runDoctor, setupService } from "./commands/index.js";
import {
  resolveRuntimeConfig,
  toEntryPointRuntimeConfig,
  type ResolvedRuntimeConfig,
} from "./config.js";
import { startAutoReindexTasks } from "./indexer.js";
import { buildHealthPayload, createDeepObsidianMcpServer } from "./server.js";
import {
  normalizeServiceConfig,
  runHttpService,
  type ResolvedServiceConfig,
  type ServiceConfigInput,
} from "./service.js";
import { HybridStdioTransport } from "./transport.js";
import { ensureVaultPath } from "./vault.js";

const VERSION = "0.1.0";

function printLine(text = "", stream: "stdout" | "stderr" = "stdout"): void {
  const target = stream === "stderr" ? process.stderr : process.stdout;
  target.write(`${text}\n`);
}

function printJson(value: unknown): void {
  printLine(JSON.stringify(value, null, 2));
}

function serviceInputFromRuntimeConfig(
  resolved: ResolvedRuntimeConfig,
  overrides?: Partial<ServiceConfigInput>,
): ServiceConfigInput {
  return {
    vaultPath: resolved.vaultPath,
    indexDir: resolved.indexDir,
    transport: resolved.transport,
    stdioMode: resolved.stdioMode,
    http: resolved.http,
    autoReindex: resolved.autoReindex,
    embedding: resolved.embedding,
    configFilePath: resolved.configFilePath,
    ...overrides,
  };
}

function printHelp(): void {
  printLine("Usage:");
  printLine("  deep-obsidian-mcp [serve] [--config <path>] [--vault <path>] [--transport stdio|http]");
  printLine("  deep-obsidian-mcp setup-service --vault <path> [--config <path>] [--dry-run]");
  printLine("  deep-obsidian-mcp doctor [--config <path>] [--json]");
  printLine("  deep-obsidian-mcp print-config [--config <path>]");
  printLine("  deep-obsidian-mcp probe [--config <path>] [--json]");
  printLine("");
  printLine("Commands:");
  printLine("  serve          Start the MCP server using resolved config.");
  printLine("  setup-service  Validate and persist HTTP service config.");
  printLine("  doctor         Diagnose config, vault access, dependencies, and health.");
  printLine("  print-config   Print the normalized persisted config.");
  printLine("  probe          Probe the configured HTTP health and MCP endpoints.");
  printLine("  help           Show this help.");
  printLine("  version        Print the current version.");
}

async function runServe(resolved: ResolvedRuntimeConfig): Promise<void> {
  const runtime = toEntryPointRuntimeConfig(resolved);
  if (!runtime.vaultPath) {
    throw new Error("Missing vault path. Pass --vault, use a config file, or set OBSIDIAN_VAULT_PATH.");
  }

  const vaultPath = await ensureVaultPath(runtime.vaultPath);
  const serverOptions = {
    vaultPath,
    indexDir: runtime.indexDir,
    embeddingConfig: runtime.embeddingConfig,
    autoReindex: runtime.autoReindex,
    reindexDebounceMs: runtime.reindexDebounceMs,
    reindexIntervalMs: runtime.reindexIntervalMs,
  };

  const autoReindexTasks = runtime.autoReindex
    ? startAutoReindexTasks(vaultPath, {
        indexDir: runtime.indexDir,
        embeddingConfig: runtime.embeddingConfig,
        debounceMs: runtime.reindexDebounceMs,
        syncIntervalMs: runtime.reindexIntervalMs,
        logger: (message) => console.error(`[auto-reindex] ${message}`),
      })
    : null;

  let shuttingDown = false;
  const shutdown = (httpServer?: ReturnType<typeof createHttpServer>): void => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    autoReindexTasks?.stop();
    httpServer?.close();
    setImmediate(() => process.exit(0));
  };

  if (runtime.transportMode === "http") {
    const resolvedServiceConfig = normalizeServiceConfig(serviceInputFromRuntimeConfig(resolved, { vaultPath }));
    await runHttpService(resolvedServiceConfig, async ({ config, endpoints }) => {
      const httpServer = createHttpServer(async (req, res) => {
        const requestUrl = new URL(req.url ?? "/", `http://${req.headers.host ?? `${config.http.host}:${config.http.port}`}`);
        if (requestUrl.pathname === config.http.healthPath) {
          if (req.method !== "GET") {
            res.writeHead(405, { "content-type": "application/json" });
            res.end(JSON.stringify({ error: "Method not allowed" }));
            return;
          }

          try {
            res.writeHead(200, { "content-type": "application/json" });
            res.end(JSON.stringify(await buildHealthPayload(serverOptions)));
          } catch (error) {
            res.writeHead(500, { "content-type": "application/json" });
            res.end(JSON.stringify({
              status: "error",
              message: error instanceof Error ? error.message : String(error),
            }));
          }
          return;
        }

        if (requestUrl.pathname !== config.http.mcpPath) {
          res.writeHead(404, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: "Not found" }));
          return;
        }

        if (req.method !== "POST") {
          res.writeHead(405, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: "Method not allowed", allowed: ["POST"] }));
          return;
        }

        const server = createDeepObsidianMcpServer(serverOptions);
        const transport = new StreamableHTTPServerTransport({
          sessionIdGenerator: undefined,
          enableJsonResponse: true,
        });

        try {
          await server.connect(transport);
          res.on("close", () => {
            void transport.close();
            void server.close();
          });
          await transport.handleRequest(req, res);
        } catch (error) {
          console.error("HTTP transport error:", error);
          if (!res.headersSent) {
            res.writeHead(500, { "content-type": "application/json" });
            res.end(JSON.stringify({
              jsonrpc: "2.0",
              error: {
                code: -32603,
                message: "Internal server error",
              },
              id: null,
            }));
          }
        }
      });

      process.once("SIGINT", () => shutdown(httpServer));
      process.once("SIGTERM", () => shutdown(httpServer));

      await new Promise<void>((resolve, reject) => {
        httpServer.once("error", reject);
        httpServer.listen(config.http.port, config.http.host, () => {
          httpServer.off("error", reject);
          resolve();
        });
      });

      await autoReindexTasks?.ready;
      console.error(
        `deep-obsidian-mcp service running at ${endpoints.mcp} (health=${config.http.healthPath}, semantic=${runtime.embeddingConfig.backend}, autoReindex=${runtime.autoReindex})`,
      );
    });
    return;
  }

  const server = createDeepObsidianMcpServer(serverOptions);
  const transport = new HybridStdioTransport(process.stdin, process.stdout, runtime.stdioMode);
  process.once("SIGINT", () => shutdown());
  process.once("SIGTERM", () => shutdown());
  process.stdin.once("close", () => shutdown());
  process.stdin.once("end", () => shutdown());
  await server.connect(transport);
  await autoReindexTasks?.ready;
  console.error(
    `deep-obsidian-mcp running for vault ${vaultPath} (stdio=${runtime.stdioMode}, semantic=${runtime.embeddingConfig.backend}, autoReindex=${runtime.autoReindex})`,
  );
}

function renderSetupServiceResult(result: Awaited<ReturnType<typeof setupService>>): void {
  for (const message of result.messages) {
    printLine(message);
  }
  printLine(`mcp endpoint: ${result.endpoints.mcp}`);
  printLine(`health endpoint: ${result.endpoints.health}`);
}

function renderDoctorReport(report: Awaited<ReturnType<typeof runDoctor>>): void {
  printLine(`config: ${report.config.configFilePath ?? "(none)"}`);
  printLine(`vault: ${report.config.vaultPath}`);
  printLine(`transport: ${report.config.transport}`);
  printLine(`mcp endpoint: ${report.endpoints.mcp}`);
  printLine(`health endpoint: ${report.endpoints.health}`);
  printLine("");
  for (const check of report.checks) {
    printLine(`[${check.status}] ${check.name}: ${check.message}`);
  }
}

function renderProbeResult(result: Awaited<ReturnType<typeof probeService>>): void {
  printLine(`mcp endpoint: ${result.endpoints.mcp}`);
  printLine(`health endpoint: ${result.endpoints.health}`);
  printLine(`health ok: ${result.health.ok}`);
  printLine(`mcp ok: ${result.mcp.ok}`);
  if (!result.health.ok && result.health.error) {
    printLine(`health error: ${result.health.error}`);
  }
  if (!result.mcp.ok && result.mcp.error) {
    printLine(`mcp error: ${result.mcp.error}`);
  }
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  if (argv.includes("--help") || argv.includes("-h")) {
    printHelp();
    return;
  }
  if (argv.length === 1 && (argv[0] === "--version" || argv[0] === "-v")) {
    printLine(VERSION);
    return;
  }

  const cli = parseCli(argv);
  if (cli.command === "help") {
    printHelp();
    return;
  }
  if (cli.command === "version") {
    printLine(VERSION);
    return;
  }

  const resolved = await resolveRuntimeConfig(cli);

  switch (cli.command) {
    case "serve": {
      await runServe(resolved);
      return;
    }
    case "setup-service": {
      const result = await setupService({
        config: serviceInputFromRuntimeConfig(resolved, { transport: "http" }),
        configFilePath: resolved.configFilePath,
        dryRun: cli.dryRun,
      });
      if (cli.json) {
        printJson(result);
      } else {
        renderSetupServiceResult(result);
      }
      return;
    }
    case "doctor": {
      const report = await runDoctor({
        config: serviceInputFromRuntimeConfig(resolved),
      });
      if (cli.json) {
        printJson(report);
      } else {
        renderDoctorReport(report);
      }
      if (!report.ok) {
        process.exitCode = 1;
      }
      return;
    }
    case "print-config": {
      const result = printConfig({
        config: serviceInputFromRuntimeConfig(resolved),
      });
      printLine(result.text.trimEnd());
      return;
    }
    case "probe": {
      const result = await probeService({
        config: serviceInputFromRuntimeConfig(resolved, { transport: "http" }),
      });
      if (cli.json) {
        printJson(result);
      } else {
        renderProbeResult(result);
      }
      if (!result.health.ok || !result.mcp.ok) {
        process.exitCode = 1;
      }
      return;
    }
    default: {
      throw new Error(`Unsupported command: ${cli.command satisfies never}`);
    }
  }
}

main().catch((error) => {
  console.error("Server error:", error);
  process.exit(1);
});
