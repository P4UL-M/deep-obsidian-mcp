import { access, mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import net from "node:net";
import path from "node:path";
import { promisify } from "node:util";

import { execFile } from "node:child_process";

const execFileAsync = promisify(execFile);

export type CheckStatus = "ok" | "warn" | "fail" | "skip";

export interface CheckResult {
  name: string;
  status: CheckStatus;
  message: string;
  details?: Record<string, unknown>;
}

export async function pathExists(targetPath: string): Promise<boolean> {
  return !!(await stat(targetPath).catch(() => null));
}

export async function ensureReadablePath(targetPath: string): Promise<void> {
  await access(targetPath, fsConstants.R_OK);
}

export async function ensureWritableDirectory(directoryPath: string): Promise<void> {
  await mkdir(directoryPath, { recursive: true });
  await access(directoryPath, fsConstants.W_OK);
}

export async function assertCreatableDirectory(directoryPath: string): Promise<void> {
  const resolvedPath = path.resolve(directoryPath);
  let current = resolvedPath;

  while (!(await pathExists(current))) {
    const parent = path.dirname(current);
    if (parent === current) {
      break;
    }
    current = parent;
  }

  const currentStat = await stat(current).catch(() => null);
  if (!currentStat || !currentStat.isDirectory()) {
    throw new Error(`Directory is not writable: ${resolvedPath}`);
  }

  await access(current, fsConstants.W_OK);
}

export async function writeJsonFile(filePath: string, value: unknown): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });
  await writeFile(filePath, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

export async function readJsonFile<T>(filePath: string): Promise<T> {
  const text = await readFile(filePath, "utf8");
  return JSON.parse(text) as T;
}

export async function checkCommandAvailable(command: string): Promise<{ available: boolean; output?: string }> {
  try {
    const result = await execFileAsync(command, ["--version"], { maxBuffer: 1024 * 1024 });
    return {
      available: true,
      output: result.stdout.trim(),
    };
  } catch {
    return { available: false };
  }
}

export async function isPortAvailable(host: string, port: number): Promise<{ available: boolean; occupiedBy?: string }> {
  return await new Promise((resolve) => {
    const server = net.createServer();
    server.unref();
    server.once("error", (error: NodeJS.ErrnoException) => {
      if (error.code === "EADDRINUSE") {
        resolve({ available: false, occupiedBy: `${host}:${port}` });
        return;
      }
      resolve({ available: false, occupiedBy: `${host}:${port}` });
    });
    server.listen({ host, port, exclusive: true }, () => {
      server.close(() => resolve({ available: true }));
    });
  });
}

export async function probeHealthUrl(url: string, timeoutMs = 5000): Promise<{
  ok: boolean;
  status?: number;
  body?: unknown;
  error?: string;
}> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, {
      method: "GET",
      signal: controller.signal,
      headers: {
        accept: "application/json",
      },
    });

    const contentType = response.headers.get("content-type") ?? "";
    let body: unknown = await response.text();
    if (contentType.includes("application/json")) {
      try {
        body = JSON.parse(body as string);
      } catch {
        // Keep raw text when JSON decoding fails.
      }
    }

    return {
      ok: response.ok,
      status: response.status,
      body,
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  } finally {
    clearTimeout(timeout);
  }
}

export function redactSecrets(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map((item) => redactSecrets(item));
  }
  if (!value || typeof value !== "object") {
    return value;
  }

  const result: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
    if (key.toLowerCase().includes("key") || key.toLowerCase().includes("secret") || key.toLowerCase().includes("token")) {
      result[key] = item ? "[redacted]" : item;
      continue;
    }
    result[key] = redactSecrets(item);
  }
  return result;
}
