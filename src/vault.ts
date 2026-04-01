import { promises as fs } from "node:fs";
import path from "node:path";

export const DEFAULT_IGNORED_DIRS = new Set([
  ".git",
  ".obsidian",
  ".trash",
  ".deep-obsidian-mcp",
  "node_modules",
]);

export async function ensureVaultPath(vaultPath: string): Promise<string> {
  const resolved = path.resolve(vaultPath);
  const stat = await fs.stat(resolved).catch(() => null);
  if (!stat || !stat.isDirectory()) {
    throw new Error(`Vault path does not exist or is not a directory: ${resolved}`);
  }
  return resolved;
}

export function ensureInsideVault(vaultPath: string, relativePath: string): string {
  const normalized = relativePath.replace(/^\/+/, "");
  const candidate = path.resolve(vaultPath, normalized);
  const relative = path.relative(vaultPath, candidate);
  if (relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new Error(`Path escapes the vault: ${relativePath}`);
  }
  return candidate;
}

export async function readTextFile(vaultPath: string, relativePath: string): Promise<{ absolutePath: string; text: string }> {
  const absolutePath = ensureInsideVault(vaultPath, relativePath);
  const text = await fs.readFile(absolutePath, "utf8");
  return { absolutePath, text };
}

export async function writeTextFile(
  vaultPath: string,
  relativePath: string,
  text: string,
): Promise<{ absolutePath: string; created: boolean }> {
  const absolutePath = ensureInsideVault(vaultPath, relativePath);
  const created = !(await fs.stat(absolutePath).catch(() => null));
  await fs.mkdir(path.dirname(absolutePath), { recursive: true });
  await fs.writeFile(absolutePath, text, "utf8");
  return { absolutePath, created };
}

export async function listMarkdownFiles(vaultPath: string): Promise<string[]> {
  const files: string[] = [];

  async function walk(currentPath: string): Promise<void> {
    const entries = await fs.readdir(currentPath, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.name.startsWith(".")) {
        continue;
      }

      const nextPath = path.join(currentPath, entry.name);
      if (entry.isDirectory()) {
        if (DEFAULT_IGNORED_DIRS.has(entry.name)) {
          continue;
        }
        await walk(nextPath);
        continue;
      }

      if (entry.isFile() && entry.name.toLowerCase().endsWith(".md")) {
        files.push(path.relative(vaultPath, nextPath).split(path.sep).join("/"));
      }
    }
  }

  await walk(vaultPath);
  files.sort((left, right) => left.localeCompare(right));
  return files;
}

export async function listTopLevelFolders(vaultPath: string): Promise<string[]> {
  const entries = await fs.readdir(vaultPath, { withFileTypes: true });
  return entries
    .filter((entry) => entry.isDirectory() && !entry.name.startsWith(".") && !DEFAULT_IGNORED_DIRS.has(entry.name))
    .map((entry) => entry.name)
    .sort((left, right) => left.localeCompare(right));
}

export function sliceLines(text: string, startLine: number, endLine: number): string {
  const lines = text.split(/\r?\n/);
  const start = Math.max(1, startLine);
  const end = Math.max(start, endLine);
  return lines.slice(start - 1, end).join("\n");
}

export function chunkLines(
  text: string,
  chunkSizeLines: number,
  overlapLines: number,
): Array<{ chunkIndex: number; startLine: number; endLine: number; text: string }> {
  const lines = text.split(/\r?\n/);
  const chunks: Array<{ chunkIndex: number; startLine: number; endLine: number; text: string }> = [];
  const safeChunkSize = Math.max(1, chunkSizeLines);
  const safeOverlap = Math.max(0, Math.min(overlapLines, safeChunkSize - 1));
  let start = 0;
  let chunkIndex = 0;

  while (start < lines.length) {
    const end = Math.min(lines.length, start + safeChunkSize);
    chunks.push({
      chunkIndex,
      startLine: start + 1,
      endLine: end,
      text: lines.slice(start, end).join("\n"),
    });
    if (end >= lines.length) {
      break;
    }
    start = end - safeOverlap;
    chunkIndex += 1;
  }

  return chunks;
}
