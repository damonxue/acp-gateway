// The chat view: the transcript, a prompt box, and the permission sheet.
//
// The permission sheet is deliberately the loudest thing on screen and "Deny"
// is the first button: this is the UI where someone approves `rm -rf`.

import { useEffect, useRef, useState } from "preact/hooks";
import type { Transcript, TranscriptItem } from "../api/transcript";
import type { AgentSession, PermissionRequest } from "../api/types";
import { Status } from "./Sessions";

interface Props {
  session: AgentSession;
  transcript: Transcript;
  connected: boolean;
  onBack(): void;
  onPrompt(text: string): void;
  onCancel(): void;
  onAnswer(requestId: string, answer: { option_id: string } | { approved: boolean }): void;
}

export function Chat({
  session,
  transcript,
  connected,
  onBack,
  onPrompt,
  onCancel,
  onAnswer,
}: Props) {
  const [draft, setDraft] = useState("");
  const bottom = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [transcript.items.length, transcript.pending.length]);

  const running = session.status === "running" || session.status === "cancelling";

  return (
    <div class="chat">
      <header>
        <button type="button" class="link" onClick={onBack}>
          ‹ Sessions
        </button>
        <div class="grow">
          <strong>{session.title ?? session.agent_name}</strong>
          <div class="muted small mono">{session.workspace}</div>
        </div>
        <Status status={session.status} />
      </header>

      <div class="transcript">
        {transcript.items.map((item) => (
          <Item key={item.id} item={item} />
        ))}
        <div ref={bottom} />
      </div>

      {transcript.pending.map((request) => (
        <PermissionSheet
          key={request.id}
          request={request}
          onAnswer={(answer) => onAnswer(request.id, answer)}
        />
      ))}

      <form
        class="composer"
        onSubmit={(event) => {
          event.preventDefault();
          const text = draft.trim();
          if (text === "" || !connected) return;
          onPrompt(text);
          setDraft("");
        }}
      >
        <textarea
          rows={1}
          value={draft}
          placeholder={connected ? "Send a prompt…" : "Offline…"}
          disabled={!connected}
          onInput={(event) => setDraft((event.target as HTMLTextAreaElement).value)}
          onKeyDown={(event) => {
            // Enter sends, shift+enter makes a newline — the convention every
            // chat app on a phone keyboard uses.
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              (event.currentTarget as HTMLTextAreaElement).form?.requestSubmit();
            }
          }}
        />
        {running ? (
          <button type="button" class="danger" onClick={onCancel}>
            Stop
          </button>
        ) : (
          <button type="submit" disabled={!connected || draft.trim() === ""}>
            Send
          </button>
        )}
      </form>
    </div>
  );
}

function Item({ item }: { item: TranscriptItem }) {
  switch (item.kind) {
    case "user":
      return <div class="bubble user">{item.text}</div>;
    case "agent":
      return <div class="bubble agent">{item.text}</div>;
    case "thought":
      return <div class="bubble thought">{item.text}</div>;
    case "tool":
      return (
        <div class={`tool ${item.status}`}>
          <div class="row">
            <span class="mono">🔧 {item.title}</span>
            <span class="muted small">{item.status}</span>
          </div>
          {item.output !== "" && <pre>{item.output}</pre>}
        </div>
      );
    case "terminal":
      return (
        <pre class={`terminal ${item.stream}`}>{item.text}</pre>
      );
    case "permission":
      return (
        <div class="bubble notice">
          {item.answer === null
            ? `Permission requested: ${item.request.title ?? "tool call"}`
            : `Permission ${item.answer}: ${item.request.title ?? "tool call"}`}
        </div>
      );
    case "notice":
      return <div class="bubble notice">{item.text}</div>;
  }
}

function PermissionSheet({
  request,
  onAnswer,
}: {
  request: PermissionRequest;
  onAnswer(answer: { option_id: string } | { approved: boolean }): void;
}) {
  const command = describeToolCall(request.tool_call) ?? request.title ?? "this operation";
  // Reject options first: on a phone the thumb rests near the bottom, and the
  // destructive choice must not be the easy one.
  const options = [...request.options].sort(
    (left, right) => Number(left.kind.startsWith("allow")) - Number(right.kind.startsWith("allow")),
  );

  return (
    <div class="sheet">
      <h2>Permission required</h2>
      <p class="muted small">The agent wants to run:</p>
      <pre>{command}</pre>
      <div class="row gap wrap">
        {options.length === 0 ? (
          <>
            <button type="button" class="danger" onClick={() => onAnswer({ approved: false })}>
              Deny
            </button>
            <button type="button" onClick={() => onAnswer({ approved: true })}>
              Allow
            </button>
          </>
        ) : (
          options.map((option) => (
            <button
              key={option.option_id}
              type="button"
              class={option.kind.startsWith("allow") ? "" : "danger"}
              onClick={() => onAnswer({ option_id: option.option_id })}
            >
              {option.name}
            </button>
          ))
        )}
      </div>
    </div>
  );
}

/** Best-effort rendering of the raw ACP tool call the agent sent. */
function describeToolCall(toolCall: unknown): string | null {
  if (typeof toolCall !== "object" || toolCall === null) return null;
  const body = toolCall as Record<string, unknown>;
  if (typeof body.title === "string" && body.title !== "") return body.title;
  const raw = body.rawInput ?? body.raw_input;
  if (typeof raw === "object" && raw !== null) {
    const input = raw as Record<string, unknown>;
    const command = input.command ?? input.cmd;
    if (typeof command === "string") return command;
    if (Array.isArray(command)) return command.join(" ");
  }
  return null;
}
