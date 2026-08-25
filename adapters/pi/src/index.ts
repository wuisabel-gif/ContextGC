/**
 * Thin Pi adapter for ContextGC.
 *
 * This module only translates events and transports JSONL. Compaction policy
 * remains in Rust. Pi-specific extension code can call this client from its
 * message/tool hooks without importing any Rust implementation details.
 */

import { spawn } from "node:child_process";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";

export type ContextKind =
  | "SystemPrompt"
  | "DeveloperPrompt"
  | "UserMessage"
  | "AssistantMessage"
  | "ToolCall"
  | "ToolResult"
  | "FileContent"
  | "CommandOutput"
  | "Error"
  | "Decision"
  | "Constraint"
  | "Checkpoint"
  | "Diff"
  | "TestResult"
  | "Other";

export interface ContextMetadata {
  file_path?: string;
  command?: string;
  exit_code?: number;
  tool_name?: string;
  artifact_ref?: string;
  recoverable?: boolean;
  pinned?: boolean;
  tags?: string[];
}

export interface ContextItem {
  id?: string;
  parent_id?: string;
  kind: ContextKind;
  content: string;
  token_count?: number;
  metadata?: ContextMetadata;
  state?: "Active" | "Resolved" | "Superseded" | "Abandoned" | "Unknown";
}

export interface ModelInfo {
  name: string;
  contextWindow: number;
  reservedOutputTokens?: number;
}

export interface ContextGCAdapterOptions {
  command?: string;
  args?: string[];
  dbPath?: string;
}

interface ProtocolResponse {
  type: string;
  request_id: string;
  error?: string;
  [key: string]: unknown;
}

type Pending = {
  resolve: (response: ProtocolResponse) => void;
  reject: (error: Error) => void;
};

export class ContextGCClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<string, Pending>();
  private sequence = 0;

  constructor(options: ContextGCAdapterOptions = {}) {
    const command = options.command ?? "contextgc";
    const args = [...(options.args ?? ["protocol"])];
    if (options.dbPath) args.push("--db", options.dbPath);
    this.child = spawn(command, args, { stdio: ["pipe", "pipe", "pipe"] });

    const lines = createInterface({ input: this.child.stdout });
    lines.on("line", (line) => this.handleResponse(line));
    this.child.stderr.on("data", (chunk: Buffer) => {
      // Keep protocol stdout machine-safe; adapters may replace this logger.
      process.stderr.write(`[contextgc] ${chunk.toString()}`);
    });
    this.child.on("error", (error) => this.rejectAll(error));
    this.child.on("exit", (code, signal) => {
      this.rejectAll(
        new Error(`contextgc exited (code=${code ?? "none"}, signal=${signal ?? "none"})`),
      );
    });
  }

  async startSession(sessionId: string, model: ModelInfo): Promise<void> {
    await this.request({
      type: "session.start",
      session_id: sessionId,
      model: {
        name: model.name,
        context_window: model.contextWindow,
        reserved_output_tokens: model.reservedOutputTokens ?? 0,
      },
    });
  }

  async add(item: ContextItem): Promise<void> {
    await this.request({ type: "context.add", item });
  }

  async plan(predictedExtraTokens = 0): Promise<ProtocolResponse> {
    return this.request({
      type: "context.plan",
      predicted_extra_tokens: predictedExtraTokens,
    });
  }

  async compact(predictedExtraTokens = 0): Promise<ProtocolResponse> {
    return this.request({
      type: "context.compact",
      predicted_extra_tokens: predictedExtraTokens,
    });
  }

  async materialize(): Promise<ProtocolResponse> {
    return this.request({ type: "context.materialize" });
  }

  async stats(): Promise<ProtocolResponse> {
    return this.request({ type: "context.stats" });
  }

  close(): void {
    this.child.stdin.end();
  }

  private request(payload: Record<string, unknown>): Promise<ProtocolResponse> {
    const requestId = `pi-${Date.now()}-${this.sequence++}`;
    const message = JSON.stringify({ ...payload, request_id: requestId });
    return new Promise<ProtocolResponse>((resolve, reject) => {
      this.pending.set(requestId, { resolve, reject });
      this.child.stdin.write(`${message}\n`, (error) => {
        if (error) {
          this.pending.delete(requestId);
          reject(error);
        }
      });
    }).then((response) => {
      if (response.type === "error") {
        throw new Error(response.error ?? "ContextGC protocol error");
      }
      return response;
    });
  }

  private handleResponse(line: string): void {
    let response: ProtocolResponse;
    try {
      response = JSON.parse(line) as ProtocolResponse;
    } catch (error) {
      this.rejectAll(new Error(`invalid ContextGC response: ${String(error)}`));
      return;
    }
    const pending = this.pending.get(response.request_id);
    if (!pending) return;
    this.pending.delete(response.request_id);
    pending.resolve(response);
  }

  private rejectAll(error: Error): void {
    for (const { reject } of this.pending.values()) reject(error);
    this.pending.clear();
  }
}

/** Start the Rust stdio server; no policy logic is duplicated here. */
export async function startContextGC(
  options?: ContextGCAdapterOptions,
): Promise<ContextGCClient> {
  return new ContextGCClient(options);
}

/**
 * Translate a Pi-like event into a generic ContextGC item.
 *
 * Harnesses can pass explicit `kind`/`metadata` for lossless mapping. The
 * fallback role mapping covers common message and tool-result event shapes.
 */
export function piMessageToContextGC(message: unknown): ContextItem {
  if (!message || typeof message !== "object") {
    throw new TypeError("Pi message must be an object");
  }
  const value = message as Record<string, unknown>;
  if (typeof value.content !== "string") {
    throw new TypeError("Pi message must contain string content");
  }
  const explicitKind = value.kind;
  const role = value.role;
  const kind: ContextKind = isContextKind(explicitKind)
    ? explicitKind
    : role === "system"
      ? "SystemPrompt"
      : role === "developer"
        ? "DeveloperPrompt"
        : role === "assistant"
          ? "AssistantMessage"
          : role === "tool"
            ? "ToolResult"
            : "UserMessage";

  return {
    id: typeof value.id === "string" ? value.id : undefined,
    kind,
    content: value.content,
    token_count: typeof value.token_count === "number" ? value.token_count : undefined,
    metadata: isMetadata(value.metadata) ? value.metadata : undefined,
  };
}

function isContextKind(value: unknown): value is ContextKind {
  return typeof value === "string" && [
    "SystemPrompt", "DeveloperPrompt", "UserMessage", "AssistantMessage",
    "ToolCall", "ToolResult", "FileContent", "CommandOutput", "Error",
    "Decision", "Constraint", "Checkpoint", "Diff", "TestResult", "Other",
  ].includes(value);
}

function isMetadata(value: unknown): value is ContextMetadata {
  return typeof value === "object" && value !== null;
}
