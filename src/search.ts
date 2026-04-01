import { execFile } from "node:child_process";
import { promisify } from "node:util";

import { ensureInsideVault, listMarkdownFiles } from "./vault.js";

const execFileAsync = promisify(execFile);

export async function findFiles(
  vaultPath: string,
  query: string,
  options?: {
    mode?: "substring" | "regex";
    limit?: number;
  },
): Promise<Array<{ path: string; matchedOn: string }>> {
  const markdownFiles = await listMarkdownFiles(vaultPath);
  const limit = Math.max(1, options?.limit ?? 20);
  const mode = options?.mode ?? "substring";
  const matcher =
    mode === "regex"
      ? new RegExp(query, "i")
      : null;
  const lowered = query.toLowerCase();

  return markdownFiles
    .filter((filePath) => {
      if (mode === "regex") {
        return matcher!.test(filePath);
      }
      return filePath.toLowerCase().includes(lowered);
    })
    .slice(0, limit)
    .map((filePath) => ({
      path: filePath,
      matchedOn: mode,
    }));
}

export async function grepSearch(
  vaultPath: string,
  query: string,
  options?: {
    regex?: boolean;
    caseSensitive?: boolean;
    glob?: string;
    contextLines?: number;
    limit?: number;
  },
): Promise<Array<{
  path: string;
  lineNumber: number;
  submatches: Array<{ start: number; end: number; text: string }>;
  lineText: string;
}>> {
  const args = [
    "--json",
    "--line-number",
    "--with-filename",
    "--hidden",
    "--glob",
    "!.obsidian/**",
    "--glob",
    "!.git/**",
    "--glob",
    "!.deep-obsidian-mcp/**",
  ];

  if (!options?.regex) {
    args.push("--fixed-strings");
  }
  if (!options?.caseSensitive) {
    args.push("--ignore-case");
  }
  if (options?.glob) {
    args.push("--glob", options.glob);
  } else {
    args.push("--glob", "*.md");
  }
  if (options?.contextLines && options.contextLines > 0) {
    args.push("--context", String(options.contextLines));
  }
  args.push(query, vaultPath);

  const { stdout } = await execFileAsync("rg", args, {
    maxBuffer: 10 * 1024 * 1024,
  }).catch((error: { code?: number; stdout?: string }) => {
    if (error.code === 1) {
      return { stdout: error.stdout ?? "" };
    }
    throw error;
  });

  const matches: Array<{
    path: string;
    lineNumber: number;
    submatches: Array<{ start: number; end: number; text: string }>;
    lineText: string;
  }> = [];
  const limit = Math.max(1, options?.limit ?? 50);

  for (const line of stdout.split(/\r?\n/)) {
    if (!line.trim()) {
      continue;
    }
    const parsed = JSON.parse(line) as Record<string, unknown>;
    if (parsed.type !== "match") {
      continue;
    }
    const data = parsed.data as {
      path: { text: string };
      line_number: number;
      submatches: Array<{ start: number; end: number; match: { text: string } }>;
      lines: { text: string };
    };
    const absolutePath = data.path.text;
    const relativePath = absolutePath.replace(`${vaultPath}/`, "");
    matches.push({
      path: relativePath,
      lineNumber: data.line_number,
      submatches: data.submatches.map((submatch) => ({
        start: submatch.start,
        end: submatch.end,
        text: submatch.match.text,
      })),
      lineText: data.lines.text.replace(/\n$/, ""),
    });
    if (matches.length >= limit) {
      break;
    }
  }

  return matches;
}

export async function fileExistsInVault(vaultPath: string, relativePath: string): Promise<boolean> {
  try {
    ensureInsideVault(vaultPath, relativePath);
    return true;
  } catch {
    return false;
  }
}
