// Folding an event log into a readable conversation.
//
// The event log is chunked: one agent answer can be hundreds of
// `agent_message_chunk` events, and one tool call arrives as a `tool_call`
// followed by any number of `tool_call_update`s. Rendering events one-to-one
// would produce an unreadable wall, so the client folds them into items — and
// this folding is a pure function of the event list, so a reconnect that
// replays events produces exactly the same transcript.

import type { AgentEvent, PermissionRequest } from "./types";

export type TranscriptItem =
  | { kind: "user"; id: string; seq: number; text: string }
  | { kind: "agent"; id: string; seq: number; text: string }
  | { kind: "thought"; id: string; seq: number; text: string }
  | {
      kind: "tool";
      id: string;
      seq: number;
      toolCallId: string;
      title: string;
      status: string;
      output: string;
    }
  | { kind: "terminal"; id: string; seq: number; stream: string; text: string }
  | { kind: "permission"; id: string; seq: number; request: PermissionRequest; answer: string | null }
  | { kind: "notice"; id: string; seq: number; text: string };

export interface Transcript {
  items: TranscriptItem[];
  /** Permission requests still awaiting an answer, newest last. */
  pending: PermissionRequest[];
  lastSeq: number;
}

/** Pull display text out of an ACP content block, whatever shape it has. */
function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (typeof content !== "object" || content === null) return "";
  const block = content as Record<string, unknown>;
  if (typeof block.text === "string") return block.text;
  if (Array.isArray(block.content)) return block.content.map(contentText).join("");
  // Non-text blocks (images, resources) are summarised rather than dropped, so
  // the user can see that *something* was sent.
  if (typeof block.type === "string") return `[${block.type}]`;
  return "";
}

function chunkText(payload: unknown): string {
  if (typeof payload !== "object" || payload === null) return "";
  const body = payload as Record<string, unknown>;
  if (Array.isArray(body.content)) return body.content.map(contentText).join("");
  return contentText(body.content);
}

function toolOutput(payload: Record<string, unknown>): string {
  const content = payload.content;
  if (!Array.isArray(content)) return "";
  return content
    .map((entry) => {
      if (typeof entry !== "object" || entry === null) return "";
      const item = entry as Record<string, unknown>;
      if (item.type === "diff") return "[diff]";
      if (item.type === "terminal") return "[terminal]";
      return contentText(item.content ?? item);
    })
    .filter(Boolean)
    .join("\n");
}

/**
 * Fold events into transcript items.
 *
 * Consecutive chunks of the same kind merge into one item; tool calls are
 * updated in place by `toolCallId`; a `permission_response` resolves the
 * matching request instead of appending a new bubble.
 */
export function fold(events: AgentEvent[]): Transcript {
  const items: TranscriptItem[] = [];
  const pending = new Map<string, PermissionRequest>();
  const toolIndex = new Map<string, number>();
  const permissionIndex = new Map<string, number>();
  let lastSeq = 0;

  const appendStreamed = (
    kind: "agent" | "thought",
    event: AgentEvent,
    text: string,
  ) => {
    if (!text) return;
    const last = items[items.length - 1];
    if (last && last.kind === kind) {
      last.text += text;
      return;
    }
    items.push({ kind, id: event.id, seq: event.seq, text });
  };

  for (const event of events) {
    lastSeq = Math.max(lastSeq, event.seq);
    const payload = (event.payload ?? {}) as Record<string, unknown>;

    switch (event.type) {
      case "user_message":
      case "user_message_chunk": {
        const text = chunkText(payload);
        if (text) items.push({ kind: "user", id: event.id, seq: event.seq, text });
        break;
      }
      case "agent_message":
      case "agent_message_chunk":
        appendStreamed("agent", event, chunkText(payload));
        break;
      case "agent_thought_chunk":
        appendStreamed("thought", event, chunkText(payload));
        break;
      case "tool_call":
      case "tool_call_update": {
        const toolCallId = String(payload.toolCallId ?? payload.tool_call_id ?? event.id);
        const existing = toolIndex.get(toolCallId);
        const title = typeof payload.title === "string" ? payload.title : undefined;
        const status = typeof payload.status === "string" ? payload.status : undefined;
        const output = toolOutput(payload);
        if (existing !== undefined) {
          const item = items[existing];
          if (item && item.kind === "tool") {
            if (title) item.title = title;
            if (status) item.status = status;
            if (output) item.output = item.output ? `${item.output}\n${output}` : output;
          }
        } else {
          toolIndex.set(toolCallId, items.length);
          items.push({
            kind: "tool",
            id: event.id,
            seq: event.seq,
            toolCallId,
            title: title ?? toolCallId,
            status: status ?? "pending",
            output,
          });
        }
        break;
      }
      case "terminal_output":
        items.push({
          kind: "terminal",
          id: event.id,
          seq: event.seq,
          stream: String(payload.stream ?? "stdout"),
          text: String(payload.content ?? ""),
        });
        break;
      case "permission_request": {
        const request = payload as unknown as PermissionRequest;
        pending.set(request.id, request);
        permissionIndex.set(request.id, items.length);
        items.push({
          kind: "permission",
          id: event.id,
          seq: event.seq,
          request,
          answer: null,
        });
        break;
      }
      case "permission_response": {
        const id = String(payload.id ?? "");
        pending.delete(id);
        const at = permissionIndex.get(id);
        if (at !== undefined) {
          const item = items[at];
          if (item && item.kind === "permission") item.answer = describeOutcome(payload.outcome);
        }
        break;
      }
      case "session_failed": {
        const reason = String(payload.reason ?? "unknown error");
        items.push({
          kind: "notice",
          id: event.id,
          seq: event.seq,
          text: payload.fatal === true ? `Session failed: ${reason}` : `Turn failed: ${reason}`,
        });
        break;
      }
      case "error":
        items.push({
          kind: "notice",
          id: event.id,
          seq: event.seq,
          text: String(payload.message ?? "error"),
        });
        break;
      default:
        // session_created / session_status / session_completed / plan_update …
        // carry no chat content; the header renders status instead.
        break;
    }
  }

  return { items, pending: [...pending.values()], lastSeq };
}

function describeOutcome(outcome: unknown): string {
  if (typeof outcome !== "object" || outcome === null) return "answered";
  const body = outcome as Record<string, unknown>;
  if (body.outcome === "cancelled") return "cancelled";
  const optionId = body.optionId ?? body.option_id;
  return typeof optionId === "string" ? optionId : "answered";
}

/** A one-line summary of a tool call, for compact rendering. */
export function toolSummary(item: TranscriptItem): string {
  return item.kind === "tool" ? `${item.title} · ${item.status}` : "";
}
