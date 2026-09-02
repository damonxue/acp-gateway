import { describe, expect, it } from "vitest";
import { fold } from "./transcript";
import { classify, type AgentEvent } from "./types";

let seq = 0;
function event(type: string, payload: unknown): AgentEvent {
  seq += 1;
  return {
    id: `evt_${seq}`,
    session_id: "sess_1",
    seq,
    timestamp: new Date().toISOString(),
    type,
    payload,
  };
}

function textChunk(text: string) {
  return { content: { type: "text", text } };
}

describe("fold", () => {
  it("merges consecutive agent chunks into one message", () => {
    seq = 0;
    const transcript = fold([
      event("user_message", { content: [{ type: "text", text: "why?" }] }),
      event("agent_message_chunk", textChunk("Looking at ")),
      event("agent_message_chunk", textChunk("your project.")),
    ]);

    expect(transcript.items).toHaveLength(2);
    expect(transcript.items[0]).toMatchObject({ kind: "user", text: "why?" });
    expect(transcript.items[1]).toMatchObject({
      kind: "agent",
      text: "Looking at your project.",
    });
    expect(transcript.lastSeq).toBe(3);
  });

  it("keeps a user message from splitting the agent's answer", () => {
    seq = 0;
    const transcript = fold([
      event("agent_message_chunk", textChunk("one")),
      event("user_message", { content: [{ type: "text", text: "stop" }] }),
      event("agent_message_chunk", textChunk("two")),
    ]);
    expect(transcript.items.map((item) => item.kind)).toEqual([
      "agent",
      "user",
      "agent",
    ]);
  });

  it("updates a tool call in place instead of appending", () => {
    seq = 0;
    const transcript = fold([
      event("tool_call", { toolCallId: "t1", title: "cargo test", status: "pending" }),
      event("tool_call_update", {
        toolCallId: "t1",
        status: "completed",
        content: [{ type: "content", content: { type: "text", text: "43 passed" } }],
      }),
    ]);

    expect(transcript.items).toHaveLength(1);
    expect(transcript.items[0]).toMatchObject({
      kind: "tool",
      title: "cargo test",
      status: "completed",
      output: "43 passed",
    });
  });

  it("tracks pending permissions and resolves them", () => {
    seq = 0;
    const request = {
      id: "perm_1",
      title: "rm -rf target",
      tool_kind: null,
      tool_call: {},
      options: [
        { option_id: "allow", name: "Allow", kind: "allow_once" },
        { option_id: "deny", name: "Deny", kind: "reject_once" },
      ],
      requested_at: new Date().toISOString(),
    };

    const waiting = fold([event("permission_request", request)]);
    expect(waiting.pending).toHaveLength(1);
    expect(waiting.items[0]).toMatchObject({ kind: "permission", answer: null });

    const answered = fold([
      event("permission_request", request),
      event("permission_response", {
        id: "perm_1",
        outcome: { outcome: "selected", optionId: "allow" },
      }),
    ]);
    expect(answered.pending).toHaveLength(0);
    expect(answered.items[0]).toMatchObject({ kind: "permission", answer: "allow" });
  });

  it("surfaces failures as notices and distinguishes fatal ones", () => {
    seq = 0;
    const transcript = fold([
      event("session_failed", { reason: "rate limited", fatal: false }),
      event("session_failed", { reason: "agent died", fatal: true }),
    ]);
    expect(transcript.items[0]).toMatchObject({ text: "Turn failed: rate limited" });
    expect(transcript.items[1]).toMatchObject({ text: "Session failed: agent died" });
  });

  it("ignores events that carry no chat content", () => {
    seq = 0;
    const transcript = fold([
      event("session_created", { agent_id: "codex" }),
      event("session_status", { status: "running" }),
      event("plan_update", { entries: [] }),
      event("session_completed", { stop_reason: "end_turn" }),
    ]);
    expect(transcript.items).toHaveLength(0);
    expect(transcript.lastSeq).toBe(4);
  });

  it("is a pure function of the events, so replay is idempotent", () => {
    seq = 0;
    const events = [
      event("agent_message_chunk", textChunk("a")),
      event("agent_message_chunk", textChunk("b")),
      event("tool_call", { toolCallId: "t", title: "x" }),
    ];
    expect(fold(events)).toEqual(fold([...events]));
  });
});

describe("classify", () => {
  it("separates control messages from events", () => {
    expect(classify({ type: "hello", protocol_version: 1 }).kind).toBe("control");
    expect(classify({ type: "subscribed", session_id: "s", last_seq: 4 }).kind).toBe(
      "control",
    );
    expect(
      classify({ type: "agent_message_chunk", session_id: "s", seq: 5, payload: {} }).kind,
    ).toBe("event");
    expect(classify({ type: "agent_message_chunk" }).kind).toBe("unknown");
    expect(classify(null).kind).toBe("unknown");
  });

  it("does not confuse the two shared type names", () => {
    // `session_created` and `error` exist as both a control message and an
    // event type; only the event carries `seq`.
    expect(
      classify({ type: "session_created", session: { id: "sess_1" } }).kind,
    ).toBe("control");
    expect(
      classify({
        type: "session_created",
        session_id: "sess_1",
        seq: 1,
        payload: { agent_id: "codex" },
      }).kind,
    ).toBe("event");

    expect(classify({ type: "error", code: "x", message: "y" }).kind).toBe("control");
    expect(
      classify({ type: "error", session_id: "s", seq: 9, payload: { message: "y" } }).kind,
    ).toBe("event");
  });
});
