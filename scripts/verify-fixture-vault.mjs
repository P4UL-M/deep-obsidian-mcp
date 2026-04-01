#!/usr/bin/env node

import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

const ROOT_DIR = fileURLToPath(new URL("..", import.meta.url));
const DEFAULT_VAULT = path.join(ROOT_DIR, "tests", "fixtures", "vault");

async function listMarkdownFiles(dir, relativeDir = "") {
  const entries = await readdir(dir, { withFileTypes: true });
  const result = [];
  for (const entry of entries) {
    const fullPath = path.join(dir, entry.name);
    const relativePath = path.posix.join(relativeDir, entry.name);
    if (entry.isDirectory()) {
      result.push(...await listMarkdownFiles(fullPath, relativePath));
      continue;
    }
    if (entry.isFile() && entry.name.toLowerCase().endsWith(".md")) {
      result.push(relativePath);
    }
  }
  return result;
}

function extractWikiLinks(text) {
  return [...text.matchAll(/\[\[([^\]]+)\]\]/g)].map((match) => match[1]);
}

async function main() {
  const vaultPath = process.argv[2] ?? DEFAULT_VAULT;
  const markdownFiles = await listMarkdownFiles(vaultPath);
  assert.deepEqual(
    markdownFiles.sort(),
    ["Home.md", "Projects/Brew Service.md", "Research/Service Contract.md"].sort(),
    "fixture vault layout changed",
  );

  const home = await readFile(path.join(vaultPath, "Home.md"), "utf8");
  const brewService = await readFile(path.join(vaultPath, "Projects", "Brew Service.md"), "utf8");
  const serviceContract = await readFile(path.join(vaultPath, "Research", "Service Contract.md"), "utf8");

  assert.deepEqual(
    extractWikiLinks(home).sort(),
    ["Projects/Brew Service", "Research/Service Contract"].sort(),
    "Home.md link set changed",
  );
  assert.ok(brewService.includes("[[Research/Service Contract]]"), "Brew Service note lost its contract link");
  assert.ok(serviceContract.includes("[[Projects/Brew Service]]"), "Service Contract note lost its return link");

  console.log(JSON.stringify({
    vaultPath,
    markdownFiles: markdownFiles.length,
    linkedPairs: 4,
    status: "ok",
  }, null, 2));
}

main().catch((error) => {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  process.exit(1);
});

export { extractWikiLinks, listMarkdownFiles };
