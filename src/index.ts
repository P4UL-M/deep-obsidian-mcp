#!/usr/bin/env node

import { createServer as createHttpServer } from "node:http";
import path from "node:path";
import { McpServer, ResourceTemplate } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import * as z from "zod/v4";

import { embedTexts, normalizeEmbeddingConfig, type EmbeddingConfig } from "./embeddings.js";
import {
  bm25SearchChunks,
  graphNeighbors,
  getSearchIndex,
  hybridSearchChunks,
  relatedNotes,
  relatedNotesWithEmbeddingsSql,
  resolveWikiLinkTarget,
  semanticSearchChunks,
  semanticSearchChunksWithQueryVectorSql,
  startAutoReindexTasks,
} from "./indexer.js";
import { findFiles, grepSearch } from "./search.js";
import { noteTitle, tokenize } from "./text.js";
import { extractBlockSections, extractHeadingSections } from "./text.js";
import { HybridStdioTransport, type StdioMode } from "./transport.js";
import {
  ensureVaultPath,
  listMarkdownFiles,
  listTopLevelFolders,
  readTextFile,
  sliceLines,
  chunkLines,
  writeTextFile,
} from "./vault.js";

function parseBooleanFlag(value: string | undefined, defaultValue: boolean): boolean {
  if (!value) {
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

function normalizeHttpPath(value: string | undefined, fallbackValue: string): string {
  const candidate = (value ?? fallbackValue).trim();
  if (!candidate || candidate === "/") {
    return "/";
  }
  return `/${candidate.replace(/^\/+/, "").replace(/\/+$/, "")}`;
}

function parseArgs(argv: string[]): {
  vaultPath: string;
  indexDir?: string;
  transportMode: "stdio" | "http";
  stdioMode: StdioMode;
  httpHost: string;
  httpPort: number;
  httpMcpPath: string;
  httpHealthPath: string;
  embeddingConfig: EmbeddingConfig;
  autoReindex: boolean;
  reindexDebounceMs: number;
  reindexIntervalMs: number;
} {
  const args = [...argv];
  let vaultPath = process.env.OBSIDIAN_VAULT_PATH ?? "";
  let indexDir: string | undefined;
  let transportMode = (process.env.MCP_TRANSPORT_MODE as "stdio" | "http" | undefined) ?? "stdio";
  let stdioMode = (process.env.MCP_STDIO_MODE as StdioMode | undefined) ?? "auto";
  let httpHost = process.env.MCP_HTTP_HOST ?? "127.0.0.1";
  let httpPort = Number(process.env.MCP_HTTP_PORT ?? 4100);
  let httpMcpPath = normalizeHttpPath(process.env.MCP_HTTP_PATH, "/mcp");
  let httpHealthPath = normalizeHttpPath(process.env.MCP_HTTP_HEALTH_PATH, "/healthz");
  let embeddingProvider = process.env.EMBEDDING_PROVIDER as EmbeddingConfig["provider"] | undefined;
  let embeddingModel = process.env.EMBEDDING_MODEL ?? process.env.OPENAI_EMBEDDING_MODEL;
  let embeddingBaseUrl = process.env.EMBEDDING_BASE_URL ?? process.env.OPENAI_BASE_URL;
  let embeddingApiKey = process.env.EMBEDDING_API_KEY ?? process.env.OPENAI_API_KEY;
  let autoReindex = parseBooleanFlag(process.env.AUTO_REINDEX, true);
  let reindexDebounceMs = Number(process.env.REINDEX_DEBOUNCE_MS ?? 1500);
  let reindexIntervalMs = Number(process.env.REINDEX_INTERVAL_MS ?? 30000);

  while (args.length > 0) {
    const current = args.shift()!;
    if (current === "--transport") {
      const next = args.shift();
      transportMode = next === "http" ? "http" : "stdio";
      continue;
    }
    if (current === "--index-dir") {
      indexDir = args.shift();
      continue;
    }
    if (current === "--stdio-mode") {
      stdioMode = (args.shift() as StdioMode | undefined) ?? "auto";
      continue;
    }
    if (current === "--host") {
      httpHost = args.shift() ?? httpHost;
      continue;
    }
    if (current === "--port") {
      httpPort = Number(args.shift() ?? httpPort);
      continue;
    }
    if (current === "--mcp-path") {
      httpMcpPath = normalizeHttpPath(args.shift(), httpMcpPath);
      continue;
    }
    if (current === "--health-path") {
      httpHealthPath = normalizeHttpPath(args.shift(), httpHealthPath);
      continue;
    }
    if (current === "--embedding-provider") {
      embeddingProvider = args.shift() as EmbeddingConfig["provider"] | undefined;
      continue;
    }
    if (current === "--embedding-model") {
      embeddingModel = args.shift();
      continue;
    }
    if (current === "--embedding-base-url") {
      embeddingBaseUrl = args.shift();
      continue;
    }
    if (current === "--embedding-api-key") {
      embeddingApiKey = args.shift();
      continue;
    }
    if (current === "--auto-reindex") {
      autoReindex = parseBooleanFlag(args.shift(), true);
      continue;
    }
    if (current === "--reindex-debounce-ms") {
      reindexDebounceMs = Number(args.shift() ?? reindexDebounceMs);
      continue;
    }
    if (current === "--reindex-interval-ms") {
      reindexIntervalMs = Number(args.shift() ?? reindexIntervalMs);
      continue;
    }
    if (!vaultPath) {
      vaultPath = current;
    }
  }

  if (!vaultPath) {
    throw new Error("Usage: deep-obsidian-mcp <vault-path> [--index-dir <dir>] [--stdio-mode auto|newline|framed]");
  }

  return {
    vaultPath,
    indexDir,
    transportMode,
    stdioMode,
    httpHost,
    httpPort,
    httpMcpPath,
    httpHealthPath,
    embeddingConfig: normalizeEmbeddingConfig({
      provider: embeddingProvider,
      model: embeddingModel,
      baseUrl: embeddingBaseUrl,
      apiKey: embeddingApiKey,
    }),
    autoReindex,
    reindexDebounceMs,
    reindexIntervalMs,
  };
}

function toTextResult(data: unknown): { content: Array<{ type: "text"; text: string }>; structuredContent: Record<string, unknown> } {
  return {
    content: [
      {
        type: "text",
        text: JSON.stringify(data, null, 2),
      },
    ],
    structuredContent: data as Record<string, unknown>,
  };
}

function decodeTemplateValue(value: string | string[] | undefined): string | undefined {
  if (Array.isArray(value)) {
    return value.length > 0 ? decodeURIComponent(value[0]) : undefined;
  }
  return value ? decodeURIComponent(value) : undefined;
}

function noteUri(notePath: string): string {
  return `obsidian://note?path=${encodeURIComponent(notePath)}`;
}

function headingUri(notePath: string, slug: string): string {
  return `obsidian://heading?path=${encodeURIComponent(notePath)}&slug=${encodeURIComponent(slug)}`;
}

function blockUri(notePath: string, id: string): string {
  return `obsidian://block?path=${encodeURIComponent(notePath)}&id=${encodeURIComponent(id)}`;
}

function noteWikiLink(notePath: string): string {
  return `[[${notePath.replace(/\.md$/i, "")}]]`;
}

function slugifyTopic(topic: string): string {
  return topic
    .trim()
    .replace(/\s+/g, " ")
    .replace(/[^\w\s-]/g, "")
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .toLowerCase() || "session";
}

function titleFromTopic(topic: string): string {
  const normalized = topic.trim().replace(/\s+/g, " ");
  if (!normalized) {
    return "session";
  }
  return normalized[0].toUpperCase() + normalized.slice(1);
}

function sessionNotePath(topic: string, folder: string): string {
  const safeFolder = folder.trim().replace(/^\/+|\/+$/g, "") || "Knowledge Capture";
  return `${safeFolder}/Session - ${slugifyTopic(topic)}.md`;
}

function fallbackTitleFromPath(notePath: string): string {
  const stem = path.posix.basename(notePath, ".md").replace(/[-_]+/g, " ").replace(/\s+/g, " ").trim();
  if (!stem) {
    return "Session";
  }
  return stem[0].toUpperCase() + stem.slice(1);
}

function extractManualNotes(content: string): string | null {
  const marker = "\n## Manual Notes\n";
  const index = content.indexOf(marker);
  if (index < 0) {
    return null;
  }
  return content.slice(index + 1).trimEnd();
}

function mergeWithManualNotes(newContent: string, existingContent: string, preserveManualNotes: boolean): string {
  const normalized = `${newContent.trimEnd()}\n`;
  if (!preserveManualNotes) {
    return normalized;
  }
  const manualNotes = extractManualNotes(existingContent);
  if (!manualNotes || normalized.includes("\n## Manual Notes\n")) {
    return normalized;
  }
  return `${normalized}\n${manualNotes}\n`;
}

function buildSessionNoteHeading(
  targetPath: string,
  content: string,
  options?: { topic?: string; existingContent?: string | null },
): string {
  if (content.trimStart().startsWith("# ")) {
    return `${content.trimEnd()}\n`;
  }

  const existingTitle = options?.existingContent
    ? noteTitle(path.posix.basename(targetPath, ".md"), options.existingContent)
    : null;
  const resolvedTitle = options?.topic
    ? `Session - ${titleFromTopic(options.topic)}`
    : existingTitle ?? fallbackTitleFromPath(targetPath);

  return `# ${resolvedTitle}\n\n${content.trim()}\n`;
}

async function recommendFolder(
  vaultPath: string,
  topic: string,
  project: string | undefined,
  embeddingConfig: EmbeddingConfig,
  indexDir: string | undefined,
): Promise<{
  folder: string;
  reason: string;
  scores: Array<{ folder: string; score: number; matchedTerms: string[]; matchingPaths: string[] }>;
}> {
  const folders = await listTopLevelFolders(vaultPath);
  if (folders.length === 0) {
    return {
      folder: "Knowledge Capture",
      reason: "no visible top-level folders found",
      scores: [],
    };
  }

  const query = [topic, project].filter(Boolean).join(" ").trim();
  const { index } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
  const queryVector =
    index.semanticBackend === "embedding"
      ? (await embedTexts([query || topic], embeddingConfig)).vectors[0]
      : undefined;
  const semanticMatches =
    index.semanticBackend === "embedding"
      ? await semanticSearchChunksWithQueryVectorSql(vaultPath, queryVector ?? [], index.chunkCount || 1, indexDir)
      : undefined;
  const matches = hybridSearchChunks(index, query || topic, Math.min(index.chunkCount || 1, 24), {
    queryVector,
    semanticMatches,
  });
  const queryTerms = new Set(tokenize(query || topic));

  const scores = folders.map((folder) => {
    const folderTerms = new Set(tokenize(folder));
    const matchedTerms = [...folderTerms].filter((term) => queryTerms.has(term));
    const matchingPaths = matches
      .map((match) => match.path)
      .filter((path, idx, list) => list.indexOf(path) === idx)
      .filter((path) => path === `${folder}.md` || path.startsWith(`${folder}/`))
      .slice(0, 6);
    const score = matchedTerms.length * 8 + matchingPaths.length * 5;
    return {
      folder,
      score,
      matchedTerms,
      matchingPaths,
    };
  }).sort((left, right) => right.score - left.score || left.folder.localeCompare(right.folder));

  const best = scores[0];
  if (!best || best.score <= 0) {
    return {
      folder: "Knowledge Capture",
      reason: "no strong folder cluster found; using default knowledge bucket",
      scores,
    };
  }

  return {
    folder: best.folder,
    reason: best.matchingPaths.length > 0 ? "matched top folder among related notes" : "matched folder name to query terms",
    scores,
  };
}

function mergeKnowledgeNote(
  bucket: Map<string, {
    path: string;
    title: string;
    wikiLink: string;
    score: number;
    reasons: string[];
    sharedLinks: string[];
  }>,
  candidate: {
    path: string;
    title: string;
    score: number;
    reasons?: string[];
    sharedLinks?: string[];
  },
): void {
  const existing = bucket.get(candidate.path);
  if (!existing) {
    bucket.set(candidate.path, {
      path: candidate.path,
      title: candidate.title,
      wikiLink: noteWikiLink(candidate.path),
      score: candidate.score,
      reasons: [...(candidate.reasons ?? [])],
      sharedLinks: [...(candidate.sharedLinks ?? [])],
    });
    return;
  }

  existing.score = Math.max(existing.score, candidate.score);
  existing.reasons = [...new Set([...existing.reasons, ...(candidate.reasons ?? [])])];
  existing.sharedLinks = [...new Set([...existing.sharedLinks, ...(candidate.sharedLinks ?? [])])].slice(0, 10);
}

async function main(): Promise<void> {
  const {
    vaultPath: rawVaultPath,
    indexDir,
    transportMode,
    stdioMode,
    httpHost,
    httpPort,
    httpMcpPath,
    httpHealthPath,
    embeddingConfig,
    autoReindex,
    reindexDebounceMs,
    reindexIntervalMs,
  } = parseArgs(process.argv.slice(2));
  const vaultPath = await ensureVaultPath(rawVaultPath);
  const autoReindexTasks = autoReindex
    ? startAutoReindexTasks(vaultPath, {
        indexDir,
        embeddingConfig,
        debounceMs: reindexDebounceMs,
        syncIntervalMs: reindexIntervalMs,
        logger: (message) => console.error(`[auto-reindex] ${message}`),
      })
    : null;

  const buildServer = (): McpServer => {
    const server = new McpServer({
      name: "deep-obsidian-mcp",
      version: "0.1.0",
    });

  server.registerResource(
    "vault-overview",
    "obsidian://vault/info",
    {
      title: "Vault Overview",
      description: "Basic metadata about the configured vault and local search index.",
      mimeType: "application/json",
    },
    async () => {
      const markdownFiles = await listMarkdownFiles(vaultPath);
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      return {
        contents: [
          {
            uri: "obsidian://vault/info",
            mimeType: "application/json",
            text: JSON.stringify(
              {
                vaultPath,
                markdownFileCount: markdownFiles.length,
                indexGeneratedAt: index.generatedAt,
                chunkCount: index.chunkCount,
                noteCount: index.noteCount,
                storageBackend: "sqlite",
                vectorSearchBackend: index.semanticBackend === "embedding" ? "sqlite-vec" : "sparse-terms",
                semanticBackend: index.semanticBackend,
                embeddingProvider: index.embeddingProvider,
                embeddingModel: index.embeddingModel,
                rebuilt,
                autoReindex,
                reindexDebounceMs,
                reindexIntervalMs,
              },
              null,
              2,
            ),
          },
        ],
      };
    },
  );

  server.registerResource(
    "note-resource",
    new ResourceTemplate("obsidian://note{?path}", {
      list: async () => {
        const markdownFiles = await listMarkdownFiles(vaultPath);
        return {
          resources: markdownFiles.map((filePath) => ({
            uri: noteUri(filePath),
            name: filePath,
            mimeType: "text/markdown",
          })),
        };
      },
      complete: {
        path: async (value) => (await listMarkdownFiles(vaultPath)).filter((filePath) => filePath.includes(value)).slice(0, 50),
      },
    }),
    {
      title: "Obsidian Note",
      description: "Read a full note from the configured vault.",
      mimeType: "text/markdown",
    },
    async (_uri, variables) => {
      const notePath = decodeTemplateValue(variables.path);
      if (!notePath) {
        throw new Error("Missing note path.");
      }
      const { text } = await readTextFile(vaultPath, notePath);
      return {
        contents: [
          {
            uri: noteUri(notePath),
            mimeType: "text/markdown",
            text,
          },
        ],
      };
    },
  );

  server.registerResource(
    "heading-resource",
    new ResourceTemplate("obsidian://heading{?path,slug}", {
      list: undefined,
      complete: {
        path: async (value) => (await listMarkdownFiles(vaultPath)).filter((filePath) => filePath.includes(value)).slice(0, 50),
      },
    }),
    {
      title: "Obsidian Heading Section",
      description: "Read the section corresponding to a heading slug within a note.",
      mimeType: "text/markdown",
    },
    async (_uri, variables) => {
      const notePath = decodeTemplateValue(variables.path);
      const slug = decodeTemplateValue(variables.slug);
      if (!notePath || !slug) {
        throw new Error("Missing heading path or slug.");
      }
      const { text } = await readTextFile(vaultPath, notePath);
      const heading = extractHeadingSections(text).find((section) => section.slug === slug);
      if (!heading) {
        throw new Error(`Heading slug not found in ${notePath}: ${slug}`);
      }
      return {
        contents: [
          {
            uri: headingUri(notePath, slug),
            mimeType: "text/markdown",
            text: heading.text,
          },
        ],
      };
    },
  );

  server.registerResource(
    "block-resource",
    new ResourceTemplate("obsidian://block{?path,id}", {
      list: undefined,
      complete: {
        path: async (value) => (await listMarkdownFiles(vaultPath)).filter((filePath) => filePath.includes(value)).slice(0, 50),
      },
    }),
    {
      title: "Obsidian Block",
      description: "Read a block identified by an Obsidian block id inside a note.",
      mimeType: "text/markdown",
    },
    async (_uri, variables) => {
      const notePath = decodeTemplateValue(variables.path);
      const id = decodeTemplateValue(variables.id);
      if (!notePath || !id) {
        throw new Error("Missing block path or id.");
      }
      const { text } = await readTextFile(vaultPath, notePath);
      const block = extractBlockSections(text).find((section) => section.id === id);
      if (!block) {
        throw new Error(`Block id not found in ${notePath}: ${id}`);
      }
      return {
        contents: [
          {
            uri: blockUri(notePath, id),
            mimeType: "text/markdown",
            text: block.text,
          },
        ],
      };
    },
  );

  server.registerTool(
    "load_knowledge",
    {
      description: "Load vault knowledge related to a conversation subject using hybrid retrieval, related-note expansion, and optional graph context.",
      inputSchema: {
        subject: z.string().describe("Conversation subject or user problem to ground against the vault."),
        project: z.string().optional().describe("Optional project, repository, or domain hint."),
        limitNotes: z.number().int().positive().max(12).default(6),
        limitChunks: z.number().int().positive().max(16).default(8),
        includeGraph: z.boolean().default(true),
        graphDepth: z.number().int().positive().max(3).default(1),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ subject, project, limitNotes, limitChunks, includeGraph, graphDepth }) => {
      const query = [subject, project].filter(Boolean).join(" ").trim();
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const queryVector =
        index.semanticBackend === "embedding"
          ? (await embedTexts([query || subject], embeddingConfig)).vectors[0]
          : undefined;
      const semanticMatches =
        index.semanticBackend === "embedding"
          ? await semanticSearchChunksWithQueryVectorSql(vaultPath, queryVector ?? [], index.chunkCount || 1, indexDir)
          : undefined;
      const chunkMatches = hybridSearchChunks(index, query || subject, limitChunks, {
        queryVector,
        semanticMatches,
      });

      const noteBucket = new Map<string, {
        path: string;
        title: string;
        wikiLink: string;
        score: number;
        reasons: string[];
        sharedLinks: string[];
      }>();

      for (const chunk of chunkMatches) {
        mergeKnowledgeNote(noteBucket, {
          path: chunk.path,
          title: chunk.title,
          score: chunk.score,
          reasons: ["top chunk match"],
        });
      }

      const seedPaths = [...new Set(chunkMatches.map((chunk) => chunk.path))].slice(0, Math.min(limitNotes, 4));
      for (const seedPath of seedPaths) {
        const related =
          index.semanticBackend === "embedding"
            ? await relatedNotesWithEmbeddingsSql(vaultPath, index, seedPath, Math.min(limitNotes, 4), indexDir)
            : relatedNotes(index, seedPath, Math.min(limitNotes, 4));
        for (const note of related) {
          mergeKnowledgeNote(noteBucket, {
            path: note.path,
            title: note.title,
            score: note.score * 0.85,
            reasons: [`related to ${seedPath}`],
            sharedLinks: note.sharedLinks,
          });
        }
      }

      const notes = [...noteBucket.values()]
        .sort((left, right) => right.score - left.score || left.path.localeCompare(right.path))
        .slice(0, limitNotes);

      const graph = includeGraph && seedPaths.length > 0
        ? graphNeighbors(index, seedPaths[0], {
            direction: "both",
            depth: graphDepth,
            limit: Math.max(20, limitNotes * 4),
          })
        : { nodes: [], edges: [] };

      return toTextResult({
        subject,
        project,
        rebuilt,
        semanticBackend: index.semanticBackend,
        notes,
        chunks: chunkMatches.map((chunk) => ({
          ...chunk,
          wikiLink: noteWikiLink(chunk.path),
        })),
        graph,
      });
    },
  );

  server.registerTool(
    "recommend_folder",
    {
      description: "Choose the most coherent top-level vault folder for a session note using indexed related-note evidence.",
      inputSchema: {
        topic: z.string().describe("Session topic."),
        project: z.string().optional().describe("Optional project or repository label."),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ topic, project }) => {
      const recommendation = await recommendFolder(vaultPath, topic, project, embeddingConfig, indexDir);
      return toTextResult(recommendation);
    },
  );

  server.registerTool(
    "vault_info",
    {
      description: "Return basic metadata about the Obsidian vault and current local semantic index state.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const markdownFiles = await listMarkdownFiles(vaultPath);
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      return toTextResult({
        vaultPath,
        markdownFileCount: markdownFiles.length,
        indexGeneratedAt: index.generatedAt,
        chunkCount: index.chunkCount,
        noteCount: index.noteCount,
        storageBackend: "sqlite",
        vectorSearchBackend: index.semanticBackend === "embedding" ? "sqlite-vec" : "sparse-terms",
        semanticBackend: index.semanticBackend,
        embeddingProvider: index.embeddingProvider,
        embeddingModel: index.embeddingModel,
        rebuilt,
        autoReindex,
        reindexDebounceMs,
        reindexIntervalMs,
      });
    },
  );

  server.registerTool(
    "upsert_session_note",
    {
      description: "Create or update a session note inside the vault using either an explicit note path or a topic-derived filename, with optional manual-notes preservation.",
      inputSchema: {
        path: z.string().optional().describe("Optional vault-relative markdown path to update explicitly. When provided, it takes precedence over topic/folder routing."),
        topic: z.string().optional().describe("Session topic used to derive the session note filename when no explicit path is provided."),
        folder: z.string().optional().describe("Target folder inside the vault when no explicit path is provided."),
        content: z.string().describe("Full markdown body to store in the session note."),
        preserveManualNotes: z.boolean().default(true),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: false,
      },
    },
    async ({ path: explicitPath, topic, folder, content, preserveManualNotes }) => {
      if (explicitPath && !explicitPath.toLowerCase().endsWith(".md")) {
        throw new Error("Explicit session note path must be a vault-relative .md file.");
      }
      if (!explicitPath && (!topic || !folder)) {
        throw new Error("upsert_session_note requires either an explicit path or both topic and folder.");
      }

      const path = explicitPath ?? sessionNotePath(topic!, folder!);
      const existing = await readTextFile(vaultPath, path).catch(() => null);
      const heading = buildSessionNoteHeading(path, content, {
        topic,
        existingContent: existing?.text ?? null,
      });
      const finalContent = existing
        ? mergeWithManualNotes(heading, existing.text, preserveManualNotes)
        : heading;
      const writeResult = await writeTextFile(vaultPath, path, finalContent);
      return toTextResult({
        action: existing ? "updated" : "created",
        path,
        wikiLink: noteWikiLink(path),
        created: writeResult.created,
      });
    },
  );

  server.registerTool(
    "read_file",
    {
      description: "Read an entire note or a specific line range from the vault.",
      inputSchema: {
        path: z.string().describe("Vault-relative markdown path."),
        startLine: z.number().int().positive().optional(),
        endLine: z.number().int().positive().optional(),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, startLine, endLine }) => {
      const { text } = await readTextFile(vaultPath, path);
      const selectedText =
        startLine || endLine ? sliceLines(text, startLine ?? 1, endLine ?? startLine ?? 1) : text;
      const lines = selectedText.split(/\r?\n/);
      return toTextResult({
        path,
        startLine: startLine ?? 1,
        endLine: endLine ?? lines.length,
        lineCount: lines.length,
        text: selectedText,
      });
    },
  );

  server.registerTool(
    "read_chunk",
    {
      description: "Read a deterministic line-based chunk from a file.",
      inputSchema: {
        path: z.string().describe("Vault-relative markdown path."),
        chunkIndex: z.number().int().min(0).default(0),
        chunkSizeLines: z.number().int().positive().default(120),
        overlapLines: z.number().int().min(0).default(20),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, chunkIndex, chunkSizeLines, overlapLines }) => {
      const { text } = await readTextFile(vaultPath, path);
      const chunks = chunkLines(text, chunkSizeLines, overlapLines);
      const chunk = chunks[chunkIndex];
      if (!chunk) {
        throw new Error(`Chunk ${chunkIndex} does not exist for ${path}. Available chunks: ${chunks.length}`);
      }
      return toTextResult({
        path,
        chunkIndex,
        chunkCount: chunks.length,
        chunkSizeLines,
        overlapLines,
        startLine: chunk.startLine,
        endLine: chunk.endLine,
        text: chunk.text,
      });
    },
  );

  server.registerTool(
    "find_files",
    {
      description: "Find markdown files by classic substring or regex path search.",
      inputSchema: {
        query: z.string().describe("Substring or regex to match against vault-relative file paths."),
        mode: z.enum(["substring", "regex"]).default("substring"),
        limit: z.number().int().positive().max(200).default(20),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ query, mode, limit }) => {
      const matches = await findFiles(vaultPath, query, { mode, limit });
      return toTextResult({
        query,
        mode,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "grep_search",
    {
      description: "Search note contents using ripgrep. Supports fixed string or regex mode.",
      inputSchema: {
        query: z.string().describe("Search pattern."),
        regex: z.boolean().default(false),
        caseSensitive: z.boolean().default(false),
        glob: z.string().optional().describe("Optional rg glob, for example 'Agent Studio/*.md'."),
        contextLines: z.number().int().min(0).max(20).default(0),
        limit: z.number().int().positive().max(500).default(50),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ query, regex, caseSensitive, glob, contextLines, limit }) => {
      const matches = await grepSearch(vaultPath, query, {
        regex,
        caseSensitive,
        glob,
        contextLines,
        limit,
      });
      return toTextResult({
        query,
        regex,
        caseSensitive,
        glob,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "build_index",
    {
      description: "Force a rebuild of the local chunk index used for semantic and related-note search.",
      inputSchema: {},
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const { index } = await getSearchIndex(vaultPath, { forceRebuild: true, indexDir, embeddingConfig });
      return toTextResult({
        rebuilt: true,
        generatedAt: index.generatedAt,
        noteCount: index.noteCount,
        chunkCount: index.chunkCount,
        semanticBackend: index.semanticBackend,
        embeddingProvider: index.embeddingProvider,
        embeddingModel: index.embeddingModel,
        embeddingDimensions: index.embeddingDimensions,
      });
    },
  );

  server.registerTool(
    "bm25_search",
    {
      description: "Search note chunks with classic BM25 lexical ranking.",
      inputSchema: {
        query: z.string().describe("Lexical query."),
        limit: z.number().int().positive().max(50).default(8),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ query, limit }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const matches = bm25SearchChunks(index, query, limit);
      return toTextResult({
        query,
        rebuilt,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "semantic_search",
    {
      description: "Search semantically similar note chunks using the local vectorized chunk index.",
      inputSchema: {
        query: z.string().describe("Natural-language search query."),
        limit: z.number().int().positive().max(50).default(8),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ query, limit }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const queryVector =
        index.semanticBackend === "embedding"
          ? (await embedTexts([query], embeddingConfig)).vectors[0]
          : undefined;
      const matches =
        index.semanticBackend === "embedding"
          ? await semanticSearchChunksWithQueryVectorSql(vaultPath, queryVector ?? [], limit, indexDir)
          : semanticSearchChunks(index, query, limit);
      return toTextResult({
        query,
        rebuilt,
        semanticBackend: index.semanticBackend,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "hybrid_search",
    {
      description: "Combine BM25 lexical ranking with semantic similarity over note chunks.",
      inputSchema: {
        query: z.string().describe("Natural-language or lexical query."),
        limit: z.number().int().positive().max(50).default(8),
        semanticWeight: z.number().min(0).max(1).default(0.6),
        bm25Weight: z.number().min(0).max(1).default(0.4),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ query, limit, semanticWeight, bm25Weight }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const queryVector =
        index.semanticBackend === "embedding"
          ? (await embedTexts([query], embeddingConfig)).vectors[0]
          : undefined;
      const semanticMatches =
        index.semanticBackend === "embedding"
          ? await semanticSearchChunksWithQueryVectorSql(vaultPath, queryVector ?? [], index.chunkCount || 1, indexDir)
          : undefined;
      const matches = hybridSearchChunks(index, query, limit, {
        semanticWeight,
        bm25Weight,
        queryVector,
        semanticMatches,
      });
      return toTextResult({
        query,
        rebuilt,
        semanticBackend: index.semanticBackend,
        semanticWeight,
        bm25Weight,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "related_notes",
    {
      description: "Return notes with similar subjects to a given note path using the local note index.",
      inputSchema: {
        path: z.string().describe("Vault-relative note path."),
        limit: z.number().int().positive().max(50).default(8),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, limit }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const matches =
        index.semanticBackend === "embedding"
          ? await relatedNotesWithEmbeddingsSql(vaultPath, index, path, limit, indexDir)
          : relatedNotes(index, path, limit);
      return toTextResult({
        path,
        rebuilt,
        semanticBackend: index.semanticBackend,
        count: matches.length,
        matches,
      });
    },
  );

  server.registerTool(
    "backlinks",
    {
      description: "List notes in the vault that link to the given note.",
      inputSchema: {
        path: z.string().describe("Vault-relative note path."),
        limit: z.number().int().positive().max(200).default(50),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, limit }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const backlinks = index.notes
        .map((note) => ({
          path: note.path,
          title: note.title,
          matchedLinks: note.links.filter((link) => resolveWikiLinkTarget(index, note.path, link) === path),
        }))
        .filter((note) => note.matchedLinks.length > 0)
        .slice(0, limit);
      return toTextResult({
        path,
        rebuilt,
        count: backlinks.length,
        backlinks,
      });
    },
  );

  server.registerTool(
    "graph_traverse",
    {
      description: "Traverse the Obsidian wiki-link graph around a note, including backlinks.",
      inputSchema: {
        path: z.string().describe("Vault-relative starting note path."),
        direction: z.enum(["incoming", "outgoing", "both"]).default("both"),
        depth: z.number().int().positive().max(6).default(1),
        limit: z.number().int().positive().max(500).default(100),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, direction, depth, limit }) => {
      const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
      const graph = graphNeighbors(index, path, { direction, depth, limit });
      return toTextResult({
        path,
        rebuilt,
        direction,
        depth,
        nodeCount: graph.nodes.length,
        edgeCount: graph.edges.length,
        nodes: graph.nodes,
        edges: graph.edges,
      });
    },
  );

    return server;
  };
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

  if (transportMode === "http") {
    const httpServer = createHttpServer(async (req, res) => {
      const requestUrl = new URL(req.url ?? "/", `http://${req.headers.host ?? `${httpHost}:${httpPort}`}`);
      if (requestUrl.pathname === httpHealthPath) {
        if (req.method !== "GET") {
          res.writeHead(405, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: "Method not allowed" }));
          return;
        }
        try {
          const markdownFiles = await listMarkdownFiles(vaultPath);
          const { index, rebuilt } = await getSearchIndex(vaultPath, { indexDir, embeddingConfig });
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify({
            status: "ok",
            vaultPath,
            markdownFileCount: markdownFiles.length,
            rebuilt,
            generatedAt: index.generatedAt,
            semanticBackend: index.semanticBackend,
            autoReindex,
          }));
        } catch (error) {
          res.writeHead(500, { "content-type": "application/json" });
          res.end(JSON.stringify({ status: "error", message: error instanceof Error ? error.message : String(error) }));
        }
        return;
      }

      if (requestUrl.pathname !== httpMcpPath) {
        res.writeHead(404, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: "Not found" }));
        return;
      }

      if (req.method !== "POST") {
        res.writeHead(405, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: "Method not allowed", allowed: ["POST"] }));
        return;
      }

      const server = buildServer();
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
      httpServer.listen(httpPort, httpHost, () => {
        httpServer.off("error", reject);
        resolve();
      });
    });
    await autoReindexTasks?.ready;
    console.error(
      `deep-obsidian-mcp service running at http://${httpHost}:${httpPort}${httpMcpPath} (health=${httpHealthPath}, semantic=${embeddingConfig.backend}, autoReindex=${autoReindex})`,
    );
    return;
  }

  const server = buildServer();
  const transport = new HybridStdioTransport(process.stdin, process.stdout, stdioMode);
  process.once("SIGINT", () => shutdown());
  process.once("SIGTERM", () => shutdown());
  process.stdin.once("close", () => shutdown());
  process.stdin.once("end", () => shutdown());
  await server.connect(transport);
  await autoReindexTasks?.ready;
  console.error(
    `deep-obsidian-mcp running for vault ${vaultPath} (stdio=${stdioMode}, semantic=${embeddingConfig.backend}, autoReindex=${autoReindex})`,
  );
}

main().catch((error) => {
  console.error("Server error:", error);
  process.exit(1);
});
