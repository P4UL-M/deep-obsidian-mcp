export interface FileSnapshot {
  path: string;
  mtimeMs: number;
  size: number;
}

export interface ChunkRecord {
  path: string;
  title: string;
  chunkIndex: number;
  startLine: number;
  endLine: number;
  text: string;
  termCounts: Record<string, number>;
  norm: number;
  tokenCount: number;
  embedding?: number[];
}

export interface NoteRecord {
  path: string;
  title: string;
  termCounts: Record<string, number>;
  norm: number;
  tokenCount: number;
  links: string[];
  embedding?: number[];
}

export interface SearchIndex {
  version: number;
  generatedAt: string;
  semanticBackend: "sparse" | "embedding";
  embeddingProvider?: string;
  embeddingModel?: string;
  embeddingDimensions?: number;
  fileSnapshots: FileSnapshot[];
  documentFrequencies: Record<string, number>;
  chunkCount: number;
  noteCount: number;
  notes: NoteRecord[];
  chunks: ChunkRecord[];
}
