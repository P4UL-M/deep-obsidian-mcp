import { ensureReadablePath, isPortAvailable, probeHealthUrl, checkCommandAvailable, type CheckResult } from "./shared.js";
import { ensureVaultPath } from "../vault.js";
import {
  buildServiceEndpoints,
  normalizeServiceConfig,
  type ServiceConfigInput,
  type ResolvedServiceConfig,
} from "../service.js";
import { assertCreatableDirectory } from "./shared.js";

export interface DoctorOptions {
  config: ServiceConfigInput;
  probeTimeoutMs?: number;
}

export interface DoctorReport {
  config: ResolvedServiceConfig;
  endpoints: ReturnType<typeof buildServiceEndpoints>;
  checks: CheckResult[];
  ok: boolean;
}

export async function runDoctor(options: DoctorOptions): Promise<DoctorReport> {
  const config = normalizeServiceConfig(options.config);
  const endpoints = buildServiceEndpoints(config);
  const checks: CheckResult[] = [];

  try {
    const vaultPath = await ensureVaultPath(config.vaultPath);
    await ensureReadablePath(vaultPath);
    checks.push({
      name: "vault",
      status: "ok",
      message: `vault is readable`,
      details: { path: vaultPath },
    });
  } catch (error) {
    checks.push({
      name: "vault",
      status: "fail",
      message: error instanceof Error ? error.message : String(error),
    });
  }

  try {
    await assertCreatableDirectory(config.indexDir);
    checks.push({
      name: "index-dir",
      status: "ok",
      message: `index directory can be created or is writable`,
      details: { path: config.indexDir },
    });
  } catch (error) {
    checks.push({
      name: "index-dir",
      status: "fail",
      message: error instanceof Error ? error.message : String(error),
    });
  }

  const rgCheck = await checkCommandAvailable("rg");
  checks.push({
    name: "rg",
    status: rgCheck.available ? "ok" : "fail",
    message: rgCheck.available ? "ripgrep is available" : "ripgrep is not available on PATH",
    details: rgCheck.output ? { version: rgCheck.output } : undefined,
  });

  if (config.transport === "http") {
    const portCheck = await isPortAvailable(config.http.host, config.http.port);
    if (portCheck.available) {
      checks.push({
        name: "http-port",
        status: "ok",
        message: `port is free; service is not running`,
        details: { host: config.http.host, port: config.http.port },
      });
      checks.push({
        name: "health",
        status: "skip",
        message: `health endpoint skipped because the service is not running`,
      });
    } else {
      const health = await probeHealthUrl(endpoints.health, options.probeTimeoutMs ?? 5000);
      checks.push({
        name: "http-port",
        status: "warn",
        message: `port is in use`,
        details: { host: config.http.host, port: config.http.port },
      });
      checks.push({
        name: "health",
        status: health.ok ? "ok" : "fail",
        message: health.ok
          ? `health endpoint responded successfully`
          : health.error ?? `health endpoint returned status ${health.status ?? "unknown"}`,
        details: health.ok ? { status: health.status, body: health.body } : { status: health.status, error: health.error },
      });
    }
  } else {
    checks.push({
      name: "http-port",
      status: "skip",
      message: `transport is ${config.transport}; HTTP port checks are skipped`,
    });
    checks.push({
      name: "health",
      status: "skip",
      message: `transport is ${config.transport}; health probe is skipped`,
    });
  }

  const ok = checks.every((check) => check.status !== "fail");
  return {
    config,
    endpoints,
    checks,
    ok,
  };
}
