// The whole client is one state machine:
//
//   loading → unpaired ──pair()──► sessions ⇄ chat
//
// There is no router: on a phone, back is a button, and a URL would only invite
// people to bookmark a page that needs a live socket.

import { useCallback, useEffect, useMemo, useRef, useState } from "preact/hooks";
import { GatewayConnection, type ConnectionState } from "../api/socket";
import { forgetDevice, loadDevice, type StoredDevice } from "../api/store";
import { fold, type Transcript } from "../api/transcript";
import type {
  AgentEvent,
  AgentSession,
  AgentSummary,
  Machine,
  ServerMessage,
} from "../api/types";
import { Chat } from "./Chat";
import { Pair } from "./Pair";
import { Sessions } from "./Sessions";

interface Screen {
  device: StoredDevice | null;
  ready: boolean;
}

export function App() {
  const [screen, setScreen] = useState<Screen>({ device: null, ready: false });
  const [state, setState] = useState<ConnectionState>("connecting");
  const [detail, setDetail] = useState<string | undefined>();
  const [machine, setMachine] = useState<Machine | null>(null);
  const [agents, setAgents] = useState<AgentSummary[]>([]);
  const [sessions, setSessions] = useState<AgentSession[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [events, setEvents] = useState<Map<string, AgentEvent[]>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const connection = useRef<GatewayConnection | null>(null);

  useEffect(() => {
    void loadDevice().then((device) => setScreen({ device, ready: true }));
  }, []);

  const onControl = useCallback((message: ServerMessage) => {
    switch (message.type) {
      case "hello":
        setMachine(message.machine);
        setAgents(message.agents);
        setSessions(message.sessions);
        break;
      case "session_list":
        setSessions(message.sessions);
        break;
      case "session_created":
        setSessions((current) => [message.session, ...current]);
        setSelected(message.session.id);
        break;
      case "subscribed":
        // The snapshot is authoritative for status and pending permissions,
        // which matters when joining a session mid-turn.
        setSessions((current) =>
          current.map((session) =>
            session.id === message.session_id
              ? { ...session, ...message.snapshot }
              : session,
          ),
        );
        break;
      case "error":
        setError(message.message);
        break;
      case "pong":
        break;
    }
  }, []);

  const onEvent = useCallback((event: AgentEvent) => {
    setEvents((current) => {
      const next = new Map(current);
      const list = next.get(event.session_id) ?? [];
      // Replay can re-deliver an event the client already has; keep the log
      // append-only and de-duplicated by seq.
      if (list.some((existing) => existing.seq === event.seq)) return current;
      next.set(event.session_id, [...list, event]);
      return next;
    });
    if (event.type === "session_status" || event.type === "session_created") {
      connection.current?.listSessions();
    }
  }, []);

  useEffect(() => {
    const device = screen.device;
    if (!device) return;
    const socket = new GatewayConnection(device, {
      onState: (next, why) => {
        setState(next);
        setDetail(why);
      },
      onControl,
      onEvent,
    });
    connection.current = socket;
    void socket.connect();
    return () => {
      socket.close();
      connection.current = null;
    };
  }, [screen.device, onControl, onEvent]);

  useEffect(() => {
    if (!selected) return;
    connection.current?.subscribe(selected);
    return () => connection.current?.unsubscribe(selected);
  }, [selected]);

  const transcript: Transcript = useMemo(
    () => fold(selected ? (events.get(selected) ?? []) : []),
    [events, selected],
  );

  if (!screen.ready) {
    return <div class="center muted">Loading…</div>;
  }

  if (!screen.device) {
    return <Pair onPaired={(device) => setScreen({ device, ready: true })} />;
  }

  const session = sessions.find((candidate) => candidate.id === selected) ?? null;

  return (
    <div class="app">
      {error !== null && (
        <button type="button" class="banner error" onClick={() => setError(null)}>
          {error} · tap to dismiss
        </button>
      )}
      {state !== "open" && (
        <div class="banner">
          {state === "connecting" ? "Connecting…" : `Offline — ${detail ?? "retrying"}`}
        </div>
      )}

      {session ? (
        <Chat
          session={session}
          transcript={transcript}
          connected={state === "open"}
          onBack={() => setSelected(null)}
          onPrompt={(text) => connection.current?.prompt(session.id, text)}
          onCancel={() => connection.current?.cancel(session.id)}
          onAnswer={(requestId, answer) =>
            connection.current?.answerPermission(session.id, requestId, answer)
          }
        />
      ) : (
        <Sessions
          machine={machine}
          device={screen.device}
          agents={agents}
          sessions={sessions}
          onSelect={setSelected}
          onRefresh={() => connection.current?.listSessions()}
          onCreate={(agentId, workspace) =>
            connection.current?.createSession(agentId, workspace)
          }
          onForget={async () => {
            await forgetDevice();
            setScreen({ device: null, ready: true });
          }}
        />
      )}
    </div>
  );
}
