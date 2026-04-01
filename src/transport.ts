import process from "node:process";

import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { JSONRPCMessageSchema, type JSONRPCMessage } from "@modelcontextprotocol/sdk/types.js";

export type StdioMode = "auto" | "newline" | "framed";

function parseJsonMessage(payload: string): JSONRPCMessage {
  return JSONRPCMessageSchema.parse(JSON.parse(payload)) as JSONRPCMessage;
}

export class HybridStdioTransport implements Transport {
  onclose?: () => void;
  onerror?: (error: Error) => void;
  onmessage?: <T extends JSONRPCMessage>(message: T) => void;

  private buffer?: Buffer;
  private started = false;
  private inputMode: "newline" | "framed" | null;
  private outputMode: "newline" | "framed";

  constructor(
    private readonly stdin: NodeJS.ReadableStream = process.stdin,
    private readonly stdout: NodeJS.WritableStream = process.stdout,
    mode: StdioMode = "auto",
  ) {
    this.inputMode = mode === "auto" ? null : mode;
    this.outputMode = mode === "framed" ? "framed" : "newline";
  }

  private readonly onData = (chunk: Buffer | string): void => {
    const nextChunk = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    this.buffer = this.buffer ? Buffer.concat([this.buffer, nextChunk]) : nextChunk;
    this.processBuffer();
  };

  private readonly onError = (error: Error): void => {
    this.onerror?.(error);
  };

  async start(): Promise<void> {
    if (this.started) {
      throw new Error("HybridStdioTransport already started.");
    }
    this.started = true;
    this.stdin.on("data", this.onData);
    this.stdin.on("error", this.onError);
  }

  async close(): Promise<void> {
    this.stdin.off("data", this.onData);
    this.stdin.off("error", this.onError);
    this.buffer = undefined;
    this.onclose?.();
  }

  async send(message: JSONRPCMessage): Promise<void> {
    const serialized = JSON.stringify(message);
    const output =
      this.outputMode === "framed"
        ? `Content-Length: ${Buffer.byteLength(serialized, "utf8")}\r\n\r\n${serialized}`
        : `${serialized}\n`;

    await new Promise<void>((resolve) => {
      if (this.stdout.write(output)) {
        resolve();
        return;
      }
      this.stdout.once("drain", resolve);
    });
  }

  private processBuffer(): void {
    while (true) {
      try {
        if (!this.inputMode) {
          const detected = this.detectInputMode();
          if (!detected) {
            return;
          }
          this.inputMode = detected;
          this.outputMode = detected;
        }

        const message =
          this.inputMode === "framed" ? this.readFramedMessage() : this.readNewlineMessage();
        if (!message) {
          return;
        }
        this.onmessage?.(message);
      } catch (error) {
        this.onerror?.(error as Error);
        return;
      }
    }
  }

  private detectInputMode(): "newline" | "framed" | null {
    if (!this.buffer || this.buffer.length === 0) {
      return null;
    }

    const preview = this.buffer.toString("utf8", 0, Math.min(this.buffer.length, 64)).trimStart();
    if (preview.startsWith("Content-Length:")) {
      return "framed";
    }
    if (preview.startsWith("{")) {
      return "newline";
    }
    return null;
  }

  private readNewlineMessage(): JSONRPCMessage | null {
    if (!this.buffer) {
      return null;
    }
    const newlineIndex = this.buffer.indexOf("\n");
    if (newlineIndex === -1) {
      return null;
    }
    const line = this.buffer.toString("utf8", 0, newlineIndex).replace(/\r$/, "");
    this.buffer = this.buffer.subarray(newlineIndex + 1);
    if (!line.trim()) {
      return null;
    }
    return parseJsonMessage(line);
  }

  private readFramedMessage(): JSONRPCMessage | null {
    if (!this.buffer) {
      return null;
    }

    const crlfHeaderEnd = this.buffer.indexOf("\r\n\r\n");
    const lfHeaderEnd = this.buffer.indexOf("\n\n");
    const headerEnd =
      crlfHeaderEnd >= 0 ? crlfHeaderEnd : lfHeaderEnd >= 0 ? lfHeaderEnd : -1;
    if (headerEnd === -1) {
      return null;
    }

    const separatorLength = crlfHeaderEnd >= 0 ? 4 : 2;
    const headerText = this.buffer.toString("utf8", 0, headerEnd);
    const headers = new Map<string, string>();
    for (const line of headerText.split(/\r?\n/)) {
      const separatorIndex = line.indexOf(":");
      if (separatorIndex === -1) {
        continue;
      }
      const key = line.slice(0, separatorIndex).trim().toLowerCase();
      const value = line.slice(separatorIndex + 1).trim();
      headers.set(key, value);
    }

    const lengthHeader = headers.get("content-length");
    if (!lengthHeader) {
      throw new Error("Missing Content-Length header.");
    }
    const contentLength = Number.parseInt(lengthHeader, 10);
    if (!Number.isFinite(contentLength) || contentLength < 0) {
      throw new Error(`Invalid Content-Length header: ${lengthHeader}`);
    }

    const bodyStart = headerEnd + separatorLength;
    const bodyEnd = bodyStart + contentLength;
    if (this.buffer.length < bodyEnd) {
      return null;
    }

    const payload = this.buffer.toString("utf8", bodyStart, bodyEnd);
    this.buffer = this.buffer.subarray(bodyEnd);
    return parseJsonMessage(payload);
  }
}
