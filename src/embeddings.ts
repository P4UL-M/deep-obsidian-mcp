export interface EmbeddingConfig {
  backend: "sparse" | "embedding";
  provider?: "openai-compatible";
  model?: string;
  baseUrl?: string;
  apiKey?: string;
  maxChars?: number;
  batchSize?: number;
}

export interface EmbeddingResult {
  vectors: number[][];
  dimensions: number;
}

export function normalizeEmbeddingConfig(config: Partial<EmbeddingConfig> | undefined): EmbeddingConfig {
  const provider = config?.provider;
  const model = config?.model;
  if (!provider || !model) {
    return { backend: "sparse" };
  }

  return {
    backend: "embedding",
    provider,
    model,
    baseUrl: config?.baseUrl ?? "https://api.openai.com/v1",
    apiKey: config?.apiKey,
    maxChars: Math.max(1000, config?.maxChars ?? 12000),
    batchSize: Math.max(1, config?.batchSize ?? 32),
  };
}

function clampText(text: string, maxChars: number): string {
  return text.length > maxChars ? text.slice(0, maxChars) : text;
}

export async function embedTexts(texts: string[], config: EmbeddingConfig): Promise<EmbeddingResult> {
  if (config.backend !== "embedding" || !config.provider || !config.model || !config.baseUrl) {
    throw new Error("Embedding backend is not configured.");
  }

  if (config.provider !== "openai-compatible") {
    throw new Error(`Unsupported embedding provider: ${config.provider}`);
  }

  const body = {
    model: config.model,
    input: texts.map((text) => clampText(text, config.maxChars ?? 12000)),
  };

  const headers: Record<string, string> = {
    "content-type": "application/json",
  };
  if (config.apiKey) {
    headers.authorization = `Bearer ${config.apiKey}`;
  }

  const response = await fetch(`${config.baseUrl.replace(/\/$/, "")}/embeddings`, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
  });

  if (!response.ok) {
    const errorText = await response.text();
    throw new Error(`Embedding request failed (${response.status}): ${errorText}`);
  }

  const payload = (await response.json()) as {
    data?: Array<{ embedding: number[]; index: number }>;
  };

  const vectors = payload.data
    ?.slice()
    .sort((left, right) => left.index - right.index)
    .map((item) => item.embedding);

  if (!vectors || vectors.length !== texts.length) {
    throw new Error("Embedding provider returned an unexpected number of vectors.");
  }

  return {
    vectors,
    dimensions: vectors[0]?.length ?? 0,
  };
}
