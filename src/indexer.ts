import { promises as fs, watch, type FSWatcher } from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { load as loadSqliteVec } from "sqlite-vec";

import { embedTexts, type EmbeddingConfig } from "./embeddings.js";
import type { ChunkRecord, FileSnapshot, NoteRecord, SearchIndex } from "./types.js";
import { countTerms, extractWikiLinks, noteTitle, tokenCount, vectorNorm } from "./text.js";
import { chunkLines, ensureInsideVault, listMarkdownFiles } from "./vault.js";

const INDEX_VERSION = 1;
const DEFAULT_CHUNK_SIZE_LINES = 80;
const DEFAULT_CHUNK_OVERLAP_LINES = 12;
const BM25_K1 = 1.2;
const BM25_B = 0.75;
const SQLITE_INDEX_FILENAME = "index.sqlite";

export interface AutoReindexTasks {
  ready: Promise<void>;
  stop: () => void;
  trigger: (reason?: string) => Promise<void>;
}

export interface AutoReindexOptions {
  indexDir?: string;
  embeddingConfig?: EmbeddingConfig;
  debounceMs?: number;
  syncIntervalMs?: number;
  logger?: (message: string) => void;
}

function indexFilePath(vaultPath: string, explicitIndexDir?: string): string {
  const baseDir = explicitIndexDir ? path.resolve(explicitIndexDir) : path.join(vaultPath, ".deep-obsidian-mcp");
  return path.join(baseDir, SQLITE_INDEX_FILENAME);
}

async function ensureIndexDir(indexPath: string): Promise<void> {
  await fs.mkdir(path.dirname(indexPath), { recursive: true });
}

async function collectSnapshots(vaultPath: string): Promise<FileSnapshot[]> {
  const markdownFiles = await listMarkdownFiles(vaultPath);
  const snapshots: FileSnapshot[] = [];
  for (const relativePath of markdownFiles) {
    const absolutePath = ensureInsideVault(vaultPath, relativePath);
    const stat = await fs.stat(absolutePath);
    snapshots.push({
      path: relativePath,
      mtimeMs: stat.mtimeMs,
      size: stat.size,
    });
  }
  return snapshots;
}

function sameSnapshots(left: FileSnapshot[], right: FileSnapshot[]): boolean {
  if (left.length !== right.length) {
    return false;
  }
  for (let index = 0; index < left.length; index += 1) {
    const a = left[index];
    const b = right[index];
    if (a.path !== b.path || a.mtimeMs !== b.mtimeMs || a.size !== b.size) {
      return false;
    }
  }
  return true;
}

export async function loadIndex(vaultPath: string, explicitIndexDir?: string): Promise<SearchIndex | null> {
  const filePath = indexFilePath(vaultPath, explicitIndexDir);
  const exists = !!(await fs.stat(filePath).catch(() => null));
  if (!exists) {
    return null;
  }

  const db = openIndexDatabase(vaultPath, explicitIndexDir);
  try {
    const metadataRows = db.prepare("SELECT key, value FROM metadata").all() as Array<{ key: string; value: string }>;
    if (metadataRows.length === 0) {
      return null;
    }
    const metadata = new Map(metadataRows.map((row) => [row.key, row.value]));
    const version = Number(metadata.get("version") ?? 0);
    if (!version) {
      return null;
    }

    const fileSnapshots = db.prepare("SELECT path, mtime_ms, size FROM file_snapshots ORDER BY path").all() as Array<{
      path: string;
      mtime_ms: number;
      size: number;
    }>;
    const documentFrequencyRows = db.prepare("SELECT term, df FROM document_frequencies ORDER BY term").all() as Array<{
      term: string;
      df: number;
    }>;
    const useVecTables = hasVectorTables(db);
    const noteRows = db.prepare(
      `
        SELECT
          n.path,
          n.title,
          n.term_counts_json,
          n.norm,
          n.token_count,
          n.links_json,
          ${useVecTables ? "vec_to_json(v.embedding)" : "NULL"} AS embedding_json
        FROM notes n
        ${useVecTables ? "LEFT JOIN note_embeddings_vec v ON v.rowid = n.id" : ""}
        ORDER BY n.path
      `,
    ).all() as Array<{
      path: string;
      title: string;
      term_counts_json: string;
      norm: number;
      token_count: number;
      links_json: string;
      embedding_json: string | null;
    }>;
    const chunkRows = db.prepare(
      `
        SELECT
          c.path,
          c.title,
          c.chunk_index,
          c.start_line,
          c.end_line,
          c.text,
          c.term_counts_json,
          c.norm,
          c.token_count,
          ${useVecTables ? "vec_to_json(v.embedding)" : "NULL"} AS embedding_json
        FROM chunks c
        ${useVecTables ? "LEFT JOIN chunk_embeddings_vec v ON v.rowid = c.id" : ""}
        ORDER BY c.path, c.chunk_index
      `,
    ).all() as Array<{
      path: string;
      title: string;
      chunk_index: number;
      start_line: number;
      end_line: number;
      text: string;
      term_counts_json: string;
      norm: number;
      token_count: number;
      embedding_json: string | null;
    }>;

    return {
      version,
      generatedAt: metadata.get("generatedAt") ?? new Date(0).toISOString(),
      semanticBackend: (metadata.get("semanticBackend") as SearchIndex["semanticBackend"] | undefined) ?? "sparse",
      embeddingProvider: metadata.get("embeddingProvider") ?? undefined,
      embeddingModel: metadata.get("embeddingModel") ?? undefined,
      embeddingDimensions: metadata.get("embeddingDimensions")
        ? Number(metadata.get("embeddingDimensions"))
        : undefined,
      fileSnapshots: fileSnapshots.map((row) => ({
        path: row.path,
        mtimeMs: row.mtime_ms,
        size: row.size,
      })),
      documentFrequencies: Object.fromEntries(documentFrequencyRows.map((row) => [row.term, row.df])),
      chunkCount: Number(metadata.get("chunkCount") ?? chunkRows.length),
      noteCount: Number(metadata.get("noteCount") ?? noteRows.length),
      notes: noteRows.map((row) => ({
        path: row.path,
        title: row.title,
        termCounts: JSON.parse(row.term_counts_json) as Record<string, number>,
        norm: row.norm,
        tokenCount: row.token_count,
        links: JSON.parse(row.links_json) as string[],
        embedding: row.embedding_json ? (JSON.parse(row.embedding_json) as number[]) : undefined,
      })),
      chunks: chunkRows.map((row) => ({
        path: row.path,
        title: row.title,
        chunkIndex: row.chunk_index,
        startLine: row.start_line,
        endLine: row.end_line,
        text: row.text,
        termCounts: JSON.parse(row.term_counts_json) as Record<string, number>,
        norm: row.norm,
        tokenCount: row.token_count,
        embedding: row.embedding_json ? (JSON.parse(row.embedding_json) as number[]) : undefined,
      })),
    };
  } finally {
    db.close();
  }
}

async function writeIndex(vaultPath: string, index: SearchIndex, explicitIndexDir?: string): Promise<void> {
  const filePath = indexFilePath(vaultPath, explicitIndexDir);
  await ensureIndexDir(filePath);
  const db = openIndexDatabase(vaultPath, explicitIndexDir);
  try {
    db.exec("BEGIN IMMEDIATE TRANSACTION");

    db.exec(
      [
        "DELETE FROM metadata",
        "DELETE FROM file_snapshots",
        "DELETE FROM document_frequencies",
        "DELETE FROM notes",
        "DELETE FROM chunks",
      ].join(";"),
    );

    const insertMetadata = db.prepare("INSERT INTO metadata (key, value) VALUES (?, ?)");
    const insertSnapshot = db.prepare("INSERT INTO file_snapshots (path, mtime_ms, size) VALUES (?, ?, ?)");
    const insertDocumentFrequency = db.prepare("INSERT INTO document_frequencies (term, df) VALUES (?, ?)");
    recreateVectorTables(db, index.embeddingDimensions);

    const insertNote = db.prepare(
      "INSERT INTO notes (id, path, title, term_counts_json, norm, token_count, links_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
    );
    const insertChunk = db.prepare(
      "INSERT INTO chunks (id, path, title, chunk_index, start_line, end_line, text, term_counts_json, norm, token_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    );
    const insertNoteEmbedding =
      index.semanticBackend === "embedding" && typeof index.embeddingDimensions === "number"
        ? db.prepare("INSERT INTO note_embeddings_vec (rowid, embedding) VALUES (?, ?)")
        : null;
    const insertChunkEmbedding =
      index.semanticBackend === "embedding" && typeof index.embeddingDimensions === "number"
        ? db.prepare("INSERT INTO chunk_embeddings_vec (rowid, embedding) VALUES (?, ?)")
        : null;

    const metadataEntries: Array<[string, string]> = [
      ["version", String(index.version)],
      ["generatedAt", index.generatedAt],
      ["semanticBackend", index.semanticBackend],
      ["chunkCount", String(index.chunkCount)],
      ["noteCount", String(index.noteCount)],
    ];
    if (index.embeddingProvider) {
      metadataEntries.push(["embeddingProvider", index.embeddingProvider]);
    }
    if (index.embeddingModel) {
      metadataEntries.push(["embeddingModel", index.embeddingModel]);
    }
    if (typeof index.embeddingDimensions === "number") {
      metadataEntries.push(["embeddingDimensions", String(index.embeddingDimensions)]);
    }

    for (const [key, value] of metadataEntries) {
      insertMetadata.run(key, value);
    }
    for (const snapshot of index.fileSnapshots) {
      insertSnapshot.run(snapshot.path, snapshot.mtimeMs, snapshot.size);
    }
    for (const [term, df] of Object.entries(index.documentFrequencies)) {
      insertDocumentFrequency.run(term, df);
    }
    for (let noteId = 0; noteId < index.notes.length; noteId += 1) {
      const note = index.notes[noteId];
      insertNote.run(
        noteId + 1,
        note.path,
        note.title,
        JSON.stringify(note.termCounts),
        note.norm,
        note.tokenCount,
        JSON.stringify(note.links),
      );
      if (insertNoteEmbedding && note.embedding) {
        insertNoteEmbedding.run(BigInt(noteId + 1), JSON.stringify(note.embedding));
      }
    }
    for (let chunkId = 0; chunkId < index.chunks.length; chunkId += 1) {
      const chunk = index.chunks[chunkId];
      insertChunk.run(
        chunkId + 1,
        chunk.path,
        chunk.title,
        chunk.chunkIndex,
        chunk.startLine,
        chunk.endLine,
        chunk.text,
        JSON.stringify(chunk.termCounts),
        chunk.norm,
        chunk.tokenCount,
      );
      if (insertChunkEmbedding && chunk.embedding) {
        insertChunkEmbedding.run(BigInt(chunkId + 1), JSON.stringify(chunk.embedding));
      }
    }

    db.exec("COMMIT");
  } catch (error) {
    try {
      db.exec("ROLLBACK");
    } catch {
      // Ignore rollback errors when the transaction was not started.
    }
    throw error;
  } finally {
    db.close();
  }
}

function initializeSchema(db: DatabaseSync): void {
  db.exec(`
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
  `);

  ensureCurrentSchema(db);
}

function tableColumns(db: DatabaseSync, tableName: string): string[] {
  return (db.prepare(`PRAGMA table_info(${tableName})`).all() as Array<{ name: string }>).map((row) => row.name);
}

function needsSchemaReset(db: DatabaseSync): boolean {
  const existingTables = new Set(
    (db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all() as Array<{ name: string }>).map((row) => row.name),
  );

  if (!existingTables.has("notes") || !existingTables.has("chunks")) {
    return false;
  }

  const noteColumns = new Set(tableColumns(db, "notes"));
  const chunkColumns = new Set(tableColumns(db, "chunks"));

  return !noteColumns.has("id") || !chunkColumns.has("id");
}

function ensureCurrentSchema(db: DatabaseSync): void {
  if (needsSchemaReset(db)) {
    // Cached index data is disposable, so reset stale tables when the on-disk schema predates the current format.
    db.exec(`
      DROP TABLE IF EXISTS metadata;
      DROP TABLE IF EXISTS file_snapshots;
      DROP TABLE IF EXISTS document_frequencies;
      DROP TABLE IF EXISTS note_embeddings_vec;
      DROP TABLE IF EXISTS chunk_embeddings_vec;
      DROP TABLE IF EXISTS notes;
      DROP TABLE IF EXISTS chunks;
    `);
  }

  db.exec(`
    CREATE TABLE IF NOT EXISTS metadata (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS file_snapshots (
      path TEXT PRIMARY KEY,
      mtime_ms REAL NOT NULL,
      size INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS document_frequencies (
      term TEXT PRIMARY KEY,
      df INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS notes (
      id INTEGER PRIMARY KEY,
      path TEXT NOT NULL UNIQUE,
      title TEXT NOT NULL,
      term_counts_json TEXT NOT NULL,
      norm REAL NOT NULL,
      token_count INTEGER NOT NULL,
      links_json TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS chunks (
      id INTEGER PRIMARY KEY,
      path TEXT NOT NULL,
      title TEXT NOT NULL,
      chunk_index INTEGER NOT NULL,
      start_line INTEGER NOT NULL,
      end_line INTEGER NOT NULL,
      text TEXT NOT NULL,
      term_counts_json TEXT NOT NULL,
      norm REAL NOT NULL,
      token_count INTEGER NOT NULL,
      UNIQUE (path, chunk_index)
    );

    CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
    CREATE INDEX IF NOT EXISTS idx_notes_path ON notes(path);
  `);
}

function openIndexDatabase(vaultPath: string, explicitIndexDir?: string): DatabaseSync {
  const db = new DatabaseSync(indexFilePath(vaultPath, explicitIndexDir), { allowExtension: true });
  initializeSchema(db);
  loadSqliteVec(db);
  return db;
}

function hasVectorTables(db: DatabaseSync): boolean {
  const row = db.prepare(
    "SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'table' AND name IN ('note_embeddings_vec', 'chunk_embeddings_vec')",
  ).get() as { count: number };
  return row.count === 2;
}

function recreateVectorTables(db: DatabaseSync, embeddingDimensions?: number): void {
  db.exec("DROP TABLE IF EXISTS note_embeddings_vec; DROP TABLE IF EXISTS chunk_embeddings_vec;");
  if (!embeddingDimensions || embeddingDimensions <= 0) {
    return;
  }
  db.exec(
    `
      CREATE VIRTUAL TABLE note_embeddings_vec USING vec0(embedding float[${embeddingDimensions}]);
      CREATE VIRTUAL TABLE chunk_embeddings_vec USING vec0(embedding float[${embeddingDimensions}]);
    `,
  );
}

function sameSemanticConfig(index: SearchIndex, embeddingConfig: EmbeddingConfig): boolean {
  if (embeddingConfig.backend === "sparse") {
    return index.semanticBackend === "sparse";
  }

  return (
    index.semanticBackend === "embedding" &&
    index.embeddingProvider === embeddingConfig.provider &&
    index.embeddingModel === embeddingConfig.model
  );
}

export async function getSearchIndex(
  vaultPath: string,
  options?: { forceRebuild?: boolean; indexDir?: string; embeddingConfig?: EmbeddingConfig },
): Promise<{ index: SearchIndex; rebuilt: boolean }> {
  const snapshots = await collectSnapshots(vaultPath);
  const currentIndex = options?.forceRebuild ? null : await loadIndex(vaultPath, options?.indexDir);
  const embeddingConfig = options?.embeddingConfig ?? { backend: "sparse" };

  if (
    currentIndex &&
    currentIndex.version === INDEX_VERSION &&
    sameSnapshots(currentIndex.fileSnapshots, snapshots) &&
    sameSemanticConfig(currentIndex, embeddingConfig)
  ) {
    return { index: currentIndex, rebuilt: false };
  }

  const rebuilt = await buildIndex(vaultPath, snapshots, embeddingConfig, options?.indexDir);
  return { index: rebuilt, rebuilt: true };
}

function shouldIgnoreWatchPath(relativePath: string | null): boolean {
  if (!relativePath) {
    return false;
  }

  const normalized = relativePath.replace(/\\/g, "/");
  const segments = normalized.split("/").filter(Boolean);
  if (segments.length === 0) {
    return false;
  }

  if (segments.some((segment) => segment.startsWith("."))) {
    return true;
  }

  if (segments.some((segment) => segment === "node_modules")) {
    return true;
  }

  const basename = segments[segments.length - 1];
  if (basename.endsWith(".md")) {
    return false;
  }

  return !basename.includes(".");
}

export function startAutoReindexTasks(vaultPath: string, options?: AutoReindexOptions): AutoReindexTasks {
  const debounceMs = Math.max(100, options?.debounceMs ?? 1500);
  const syncIntervalMs = Math.max(1000, options?.syncIntervalMs ?? 30000);
  const log = options?.logger ?? (() => undefined);

  let stopped = false;
  let watcher: FSWatcher | null = null;
  let debounceTimer: NodeJS.Timeout | null = null;
  let intervalTimer: NodeJS.Timeout | null = null;
  let inflight: Promise<void> | null = null;
  let queuedReason: string | null = null;

  const run = async (reason: string, forceRebuild = false): Promise<void> => {
    if (stopped) {
      return;
    }

    if (inflight) {
      queuedReason = forceRebuild ? `${reason} (forced)` : reason;
      await inflight;
      if (!stopped && queuedReason) {
        const nextReason = queuedReason;
        queuedReason = null;
        if (nextReason !== reason) {
          await run(nextReason, forceRebuild);
        }
      }
      return;
    }

    inflight = (async () => {
      try {
        const result = await getSearchIndex(vaultPath, {
          forceRebuild,
          indexDir: options?.indexDir,
          embeddingConfig: options?.embeddingConfig,
        });
        log(
          result.rebuilt
            ? `index rebuilt (${reason}) at ${result.index.generatedAt}`
            : `index checked (${reason}); unchanged`,
        );
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        log(`index refresh failed (${reason}): ${message}`);
      } finally {
        inflight = null;
      }
    })();

    await inflight;
  };

  const schedule = (reason: string): void => {
    if (stopped) {
      return;
    }
    if (debounceTimer) {
      clearTimeout(debounceTimer);
    }
    debounceTimer = setTimeout(() => {
      debounceTimer = null;
      void run(reason);
    }, debounceMs);
  };

  const ready = run("startup");

  try {
    watcher = watch(vaultPath, { recursive: true }, (_eventType, filename) => {
      const relativePath = typeof filename === "string" ? filename : null;
      if (shouldIgnoreWatchPath(relativePath)) {
        return;
      }
      schedule(relativePath ? `watch:${relativePath}` : "watch:unknown");
    });
    watcher.on("error", (error) => {
      const message = error instanceof Error ? error.message : String(error);
      log(`watch runtime failed: ${message}; continuing with periodic sync only`);
      watcher?.close();
      watcher = null;
    });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    log(`watch setup failed: ${message}`);
  }

  intervalTimer = setInterval(() => {
    void run("periodic-sync");
  }, syncIntervalMs);

  return {
    ready,
    stop: () => {
      stopped = true;
      if (debounceTimer) {
        clearTimeout(debounceTimer);
        debounceTimer = null;
      }
      if (intervalTimer) {
        clearInterval(intervalTimer);
        intervalTimer = null;
      }
      watcher?.close();
      watcher = null;
    },
    trigger: async (reason = "manual") => {
      await run(reason, true);
    },
  };
}

async function applyEmbeddings<T extends { embedding?: number[] }>(
  records: T[],
  texts: string[],
  embeddingConfig: EmbeddingConfig,
): Promise<number | undefined> {
  if (embeddingConfig.backend !== "embedding") {
    return undefined;
  }
  if (records.length !== texts.length) {
    throw new Error("Embedding inputs must align with the target records.");
  }

  const batchSize = embeddingConfig.batchSize ?? 32;
  let dimensions: number | undefined;
  for (let index = 0; index < records.length; index += batchSize) {
    const batch = records.slice(index, index + batchSize);
    const batchTexts = texts.slice(index, index + batchSize);
    const result = await embedTexts(
      batchTexts,
      embeddingConfig,
    );
    dimensions = result.dimensions;
    result.vectors.forEach((vector, vectorIndex) => {
      batch[vectorIndex].embedding = normalizeDenseVector(vector);
    });
  }

  return dimensions;
}

function normalizeDenseVector(vector: number[]): number[] {
  const norm = Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0));
  if (!Number.isFinite(norm) || norm === 0) {
    return vector.slice();
  }
  return vector.map((value) => value / norm);
}

async function buildIndex(
  vaultPath: string,
  snapshots: FileSnapshot[],
  embeddingConfig: EmbeddingConfig,
  explicitIndexDir?: string,
): Promise<SearchIndex> {
  const chunks: ChunkRecord[] = [];
  const notes: NoteRecord[] = [];
  const noteEmbeddingInputs: string[] = [];
  const chunkEmbeddingInputs: string[] = [];
  const documentFrequencies: Record<string, number> = {};

  for (const snapshot of snapshots) {
    const absolutePath = ensureInsideVault(vaultPath, snapshot.path);
    const content = await fs.readFile(absolutePath, "utf8");
    const title = noteTitle(path.basename(snapshot.path, ".md"), content);
    const noteTermCounts = countTerms(`${title}\n${content}`);
    const noteLinks = extractWikiLinks(content);

    for (const term of new Set(Object.keys(noteTermCounts))) {
      documentFrequencies[term] = (documentFrequencies[term] ?? 0) + 1;
    }

    notes.push({
      path: snapshot.path,
      title,
      termCounts: noteTermCounts,
      norm: vectorNorm(noteTermCounts),
      tokenCount: tokenCount(noteTermCounts),
      links: noteLinks,
    });
    noteEmbeddingInputs.push(`${title}\n${content}`);

    for (const chunk of chunkLines(content, DEFAULT_CHUNK_SIZE_LINES, DEFAULT_CHUNK_OVERLAP_LINES)) {
      const termCounts = countTerms(`${title}\n${chunk.text}`);
      chunks.push({
        path: snapshot.path,
        title,
        chunkIndex: chunk.chunkIndex,
        startLine: chunk.startLine,
        endLine: chunk.endLine,
        text: chunk.text,
        termCounts,
        norm: vectorNorm(termCounts),
        tokenCount: tokenCount(termCounts),
      });
      chunkEmbeddingInputs.push(`${title}\n${chunk.text}`);
    }
  }

  const embeddingDimensions = await applyEmbeddings(
    notes,
    noteEmbeddingInputs,
    embeddingConfig,
  );
  await applyEmbeddings(
    chunks,
    chunkEmbeddingInputs,
    embeddingConfig,
  );

  const index: SearchIndex = {
    version: INDEX_VERSION,
    generatedAt: new Date().toISOString(),
    semanticBackend: embeddingConfig.backend,
    embeddingProvider: embeddingConfig.provider,
    embeddingModel: embeddingConfig.model,
    embeddingDimensions,
    fileSnapshots: snapshots,
    documentFrequencies,
    chunkCount: chunks.length,
    noteCount: notes.length,
    notes,
    chunks,
  };

  await writeIndex(vaultPath, index, explicitIndexDir);
  return index;
}

function cosineSimilarity(
  queryTermCounts: Record<string, number>,
  queryNorm: number,
  termCounts: Record<string, number>,
  termNorm: number,
): number {
  if (queryNorm === 0 || termNorm === 0) {
    return 0;
  }

  let dot = 0;
  for (const [term, queryValue] of Object.entries(queryTermCounts)) {
    const candidateValue = termCounts[term];
    if (!candidateValue) {
      continue;
    }
    dot += queryValue * candidateValue;
  }
  return dot / (queryNorm * termNorm);
}

function bm25Score(
  queryTerms: string[],
  termCounts: Record<string, number>,
  documentFrequencies: Record<string, number>,
  documentCount: number,
  documentLength: number,
  averageDocumentLength: number,
): number {
  if (documentLength <= 0 || averageDocumentLength <= 0 || documentCount <= 0) {
    return 0;
  }

  let score = 0;
  const uniqueTerms = new Set(queryTerms);
  for (const term of uniqueTerms) {
    const tf = termCounts[term] ?? 0;
    if (tf === 0) {
      continue;
    }
    const df = documentFrequencies[term] ?? 0;
    const idf = Math.log(1 + (documentCount - df + 0.5) / (df + 0.5));
    const denominator = tf + BM25_K1 * (1 - BM25_B + BM25_B * (documentLength / averageDocumentLength));
    score += idf * ((tf * (BM25_K1 + 1)) / denominator);
  }

  return score;
}

function normalizeScored<T extends { score: number }>(items: T[]): Array<T & { normalizedScore: number }> {
  const maxScore = Math.max(0, ...items.map((item) => item.score));
  return items.map((item) => ({
    ...item,
    normalizedScore: maxScore > 0 ? item.score / maxScore : 0,
  }));
}

function average(values: number[]): number {
  if (values.length === 0) {
    return 0;
  }
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

export function semanticSearchChunks(index: SearchIndex, query: string, limit: number): Array<{
  path: string;
  title: string;
  chunkIndex: number;
  startLine: number;
  endLine: number;
  score: number;
  text: string;
}> {
  if (index.semanticBackend === "embedding") {
    throw new Error("Embedding-backed search requires semanticSearchChunksWithQueryVector.");
  }

  const queryTermCounts = countTerms(query);
  const queryNorm = vectorNorm(queryTermCounts);

  return index.chunks
    .map((chunk) => ({
      path: chunk.path,
      title: chunk.title,
      chunkIndex: chunk.chunkIndex,
      startLine: chunk.startLine,
      endLine: chunk.endLine,
      text: chunk.text,
      score: cosineSimilarity(queryTermCounts, queryNorm, chunk.termCounts, chunk.norm),
    }))
    .filter((chunk) => chunk.score > 0)
    .sort((left, right) => right.score - left.score)
    .slice(0, limit);
}

export function bm25SearchChunks(index: SearchIndex, query: string, limit: number): Array<{
  path: string;
  title: string;
  chunkIndex: number;
  startLine: number;
  endLine: number;
  score: number;
  text: string;
}> {
  const queryTerms = Object.keys(countTerms(query));
  const averageChunkLength = average(index.chunks.map((chunk) => chunk.tokenCount));

  return index.chunks
    .map((chunk) => ({
      path: chunk.path,
      title: chunk.title,
      chunkIndex: chunk.chunkIndex,
      startLine: chunk.startLine,
      endLine: chunk.endLine,
      text: chunk.text,
      score: bm25Score(
        queryTerms,
        chunk.termCounts,
        index.documentFrequencies,
        index.chunkCount,
        chunk.tokenCount,
        averageChunkLength,
      ),
    }))
    .filter((chunk) => chunk.score > 0)
    .sort((left, right) => right.score - left.score)
    .slice(0, limit);
}

export async function semanticSearchChunksWithQueryVectorSql(
  vaultPath: string,
  queryVector: number[],
  limit: number,
  explicitIndexDir?: string,
): Promise<Array<{
  path: string;
  title: string;
  chunkIndex: number;
  startLine: number;
  endLine: number;
  score: number;
  text: string;
}>> {
  const db = openIndexDatabase(vaultPath, explicitIndexDir);
  try {
    const normalizedVector = JSON.stringify(normalizeDenseVector(queryVector));
    const rows = db.prepare(
      `
        SELECT
          c.path,
          c.title,
          c.chunk_index,
          c.start_line,
          c.end_line,
          c.text,
          matches.distance
        FROM (
          SELECT rowid, distance
          FROM chunk_embeddings_vec
          WHERE embedding MATCH ? AND k = ?
        ) matches
        JOIN chunks c ON c.id = matches.rowid
        ORDER BY matches.distance
      `,
    ).all(normalizedVector, limit) as Array<{
      path: string;
      title: string;
      chunk_index: number;
      start_line: number;
      end_line: number;
      text: string;
      distance: number;
    }>;
    return rows
      .map((row) => ({
        path: row.path,
        title: row.title,
        chunkIndex: row.chunk_index,
        startLine: row.start_line,
        endLine: row.end_line,
        text: row.text,
        score: 1 / (1 + row.distance),
      }));
  } finally {
    db.close();
  }
}

export function hybridSearchChunks(
  index: SearchIndex,
  query: string,
  limit: number,
  options?: {
    semanticWeight?: number;
    bm25Weight?: number;
    queryVector?: number[];
    semanticMatches?: Array<{
      path: string;
      title: string;
      chunkIndex: number;
      startLine: number;
      endLine: number;
      score: number;
      text: string;
    }>;
  },
): Array<{
  path: string;
  title: string;
  chunkIndex: number;
  startLine: number;
  endLine: number;
  score: number;
  semanticScore: number;
  bm25Score: number;
  text: string;
}> {
  const semanticWeight = options?.semanticWeight ?? 0.6;
  const bm25Weight = options?.bm25Weight ?? 0.4;

  const semanticMatches =
    options?.semanticMatches
      ? normalizeScored(options.semanticMatches)
      : index.semanticBackend === "embedding"
      ? []
      : normalizeScored(semanticSearchChunks(index, query, index.chunkCount));
  const bm25Matches = normalizeScored(bm25SearchChunks(index, query, index.chunkCount));

  const combined = new Map<string, {
    path: string;
    title: string;
    chunkIndex: number;
    startLine: number;
    endLine: number;
    text: string;
    semanticScore: number;
    bm25Score: number;
  }>();

  for (const match of semanticMatches) {
    const key = `${match.path}#${match.chunkIndex}`;
    combined.set(key, {
      path: match.path,
      title: match.title,
      chunkIndex: match.chunkIndex,
      startLine: match.startLine,
      endLine: match.endLine,
      text: match.text,
      semanticScore: match.normalizedScore,
      bm25Score: 0,
    });
  }
  for (const match of bm25Matches) {
    const key = `${match.path}#${match.chunkIndex}`;
    const current = combined.get(key);
    if (current) {
      current.bm25Score = match.normalizedScore;
      continue;
    }
    combined.set(key, {
      path: match.path,
      title: match.title,
      chunkIndex: match.chunkIndex,
      startLine: match.startLine,
      endLine: match.endLine,
      text: match.text,
      semanticScore: 0,
      bm25Score: match.normalizedScore,
    });
  }

  return [...combined.values()]
    .map((match) => ({
      ...match,
      score: semanticWeight * match.semanticScore + bm25Weight * match.bm25Score,
    }))
    .filter((match) => match.score > 0)
    .sort((left, right) => right.score - left.score)
    .slice(0, limit);
}

export function relatedNotes(index: SearchIndex, notePath: string, limit: number): Array<{
  path: string;
  title: string;
  score: number;
  sharedLinks: string[];
}> {
  if (index.semanticBackend === "embedding") {
    throw new Error("Embedding-backed related note search requires relatedNotesWithEmbeddings.");
  }

  const note = index.notes.find((candidate) => candidate.path === notePath);
  if (!note) {
    throw new Error(`Note not found in index: ${notePath}`);
  }

  return index.notes
    .filter((candidate) => candidate.path !== notePath)
    .map((candidate) => {
      const score = cosineSimilarity(note.termCounts, note.norm, candidate.termCounts, candidate.norm);
      const noteLinks = new Set(note.links);
      const sharedLinks = candidate.links.filter((link) => noteLinks.has(link)).slice(0, 10);
      return {
        path: candidate.path,
        title: candidate.title,
        score,
        sharedLinks,
      };
    })
    .filter((candidate) => candidate.score > 0)
    .sort((left, right) => right.score - left.score)
    .slice(0, limit);
}

export async function relatedNotesWithEmbeddingsSql(
  vaultPath: string,
  index: SearchIndex,
  notePath: string,
  limit: number,
  explicitIndexDir?: string,
): Promise<Array<{
  path: string;
  title: string;
  score: number;
  sharedLinks: string[];
}>> {
  const note = index.notes.find((candidate) => candidate.path === notePath);
  if (!note) {
    throw new Error(`Note not found in index: ${notePath}`);
  }
  const noteLinks = new Set(note.links);
  const db = openIndexDatabase(vaultPath, explicitIndexDir);
  try {
    const source = db.prepare(
      `
        SELECT vec_to_json(v.embedding) AS embedding_json
        FROM note_embeddings_vec v
        JOIN notes n ON n.id = v.rowid
        WHERE n.path = ?
      `,
    ).get(notePath) as { embedding_json?: string } | undefined;
    if (!source?.embedding_json) {
      throw new Error(`Note ${notePath} does not have an embedding in the current index.`);
    }
    const rows = db.prepare(
      `
        SELECT
          n.path,
          n.title,
          matches.distance,
          n.links_json
        FROM (
          SELECT rowid, distance
          FROM note_embeddings_vec
          WHERE embedding MATCH ? AND k = ?
        ) matches
        JOIN notes n ON n.id = matches.rowid
        WHERE n.path <> ?
        ORDER BY matches.distance
        LIMIT ?
      `,
    ).all(source.embedding_json, limit + 1, notePath, limit) as Array<{
      path: string;
      title: string;
      distance: number;
      links_json: string;
    }>;
    return rows
      .slice(0, limit)
      .map((row) => {
        const links = JSON.parse(row.links_json) as string[];
        return {
          path: row.path,
          title: row.title,
          score: 1 / (1 + row.distance),
          sharedLinks: links.filter((link) => noteLinks.has(link)).slice(0, 10),
        };
      });
  } finally {
    db.close();
  }
}

function stripMdExtension(notePath: string): string {
  return notePath.toLowerCase().endsWith(".md") ? notePath.slice(0, -3) : notePath;
}

export function resolveWikiLinkTarget(index: SearchIndex, sourcePath: string, rawLink: string): string | null {
  const clean = rawLink.split("#", 1)[0].trim();
  if (!clean) {
    return null;
  }

  const byExactPath = index.notes.find((note) => stripMdExtension(note.path) === stripMdExtension(clean));
  if (byExactPath) {
    return byExactPath.path;
  }

  const sourceDir = path.posix.dirname(sourcePath);
  const relativeCandidate = stripMdExtension(path.posix.join(sourceDir, clean));
  const byRelativePath = index.notes.find((note) => stripMdExtension(note.path) === relativeCandidate);
  if (byRelativePath) {
    return byRelativePath.path;
  }

  const byStem = index.notes.filter((note) => stripMdExtension(path.posix.basename(note.path)) === stripMdExtension(path.posix.basename(clean)));
  if (byStem.length === 1) {
    return byStem[0].path;
  }

  const byTitle = index.notes.filter((note) => note.title.toLowerCase() === clean.toLowerCase());
  if (byTitle.length === 1) {
    return byTitle[0].path;
  }

  return null;
}

export function getOutgoingEdges(index: SearchIndex): Array<{ source: string; target: string; rawLink: string }> {
  const edges: Array<{ source: string; target: string; rawLink: string }> = [];
  for (const note of index.notes) {
    for (const rawLink of note.links) {
      const target = resolveWikiLinkTarget(index, note.path, rawLink);
      if (!target) {
        continue;
      }
      edges.push({
        source: note.path,
        target,
        rawLink,
      });
    }
  }
  return edges;
}

export function graphNeighbors(
  index: SearchIndex,
  notePath: string,
  options?: { direction?: "incoming" | "outgoing" | "both"; depth?: number; limit?: number },
): {
  nodes: Array<{ path: string; title: string; depth: number }>;
  edges: Array<{ source: string; target: string; rawLink: string }>;
} {
  const direction = options?.direction ?? "both";
  const maxDepth = Math.max(1, options?.depth ?? 1);
  const limit = Math.max(1, options?.limit ?? 100);
  const noteByPath = new Map(index.notes.map((note) => [note.path, note]));
  if (!noteByPath.has(notePath)) {
    throw new Error(`Note not found in index: ${notePath}`);
  }

  const outgoing = getOutgoingEdges(index);
  const incoming = outgoing.map((edge) => ({ source: edge.target, target: edge.source, rawLink: edge.rawLink }));
  const adjacency = new Map<string, Array<{ source: string; target: string; rawLink: string }>>();

  const chosenEdges = [
    ...(direction === "outgoing" || direction === "both" ? outgoing : []),
    ...(direction === "incoming" || direction === "both" ? incoming : []),
  ];
  for (const edge of chosenEdges) {
    const existing = adjacency.get(edge.source) ?? [];
    existing.push(edge);
    adjacency.set(edge.source, existing);
  }

  const visited = new Map<string, number>([[notePath, 0]]);
  const queue: string[] = [notePath];
  const traversedEdges: Array<{ source: string; target: string; rawLink: string }> = [];

  while (queue.length > 0 && visited.size < limit) {
    const current = queue.shift()!;
    const currentDepth = visited.get(current) ?? 0;
    if (currentDepth >= maxDepth) {
      continue;
    }

    for (const edge of adjacency.get(current) ?? []) {
      traversedEdges.push(edge);
      if (!visited.has(edge.target)) {
        visited.set(edge.target, currentDepth + 1);
        queue.push(edge.target);
        if (visited.size >= limit) {
          break;
        }
      }
    }
  }

  return {
    nodes: [...visited.entries()].map(([pathValue, depth]) => ({
      path: pathValue,
      title: noteByPath.get(pathValue)?.title ?? pathValue,
      depth,
    })),
    edges: traversedEdges.filter((edge) => visited.has(edge.source) && visited.has(edge.target)),
  };
}
