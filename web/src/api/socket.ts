// The live connection to the gateway.
//
// Responsibilities, in order of importance:
//
//  1. never lose or duplicate an event — resubscribe with the stored cursor and
//     let the gateway replay, which is the guarantee it is built around;
//  2. survive a phone changing networks — reconnect with backoff, fetching a
//     fresh single-use ticket each time;
//  3. stay dumb — no caching or reordering here, because the event log already
//     is the source of truth.

import { fetchTicket, apiBase, socketUrl } from "./gateway";
import { loadCursor, saveCursor, type StoredDevice } from "./store";
import { classify, type AgentEvent, type ServerMessage } from "./types";

export type ConnectionState = "connecting" | "open" | "offline";

export interface SocketHandlers {
  onState(state: ConnectionState, detail?: string): void;
  onControl(message: ServerMessage): void;
  onEvent(event: AgentEvent): void;
}

const MIN_BACKOFF_MS = 500;
const MAX_BACKOFF_MS = 15_000;

export class GatewayConnection {
  private socket: WebSocket | null = null;
  private backoff = MIN_BACKOFF_MS;
  private closed = false;
  private subscriptions = new Set<string>();
  private reconnectTimer: number | null = null;

  constructor(
    private readonly device: StoredDevice,
    private readonly handlers: SocketHandlers,
  ) {}

  async connect(): Promise<void> {
    if (this.closed) return;
    this.handlers.onState("connecting");
    try {
      const ticket = await fetchTicket(this.device);
      const socket = new WebSocket(socketUrl(apiBase(this.device), ticket));
      this.socket = socket;

      socket.onopen = () => {
        this.backoff = MIN_BACKOFF_MS;
        this.handlers.onState("open");
        // Re-subscribe to whatever the UI was watching, from the cursor:
        // this is what makes a network change invisible to the user.
        for (const sessionId of this.subscriptions) {
          this.sendSubscribe(sessionId);
        }
      };

      socket.onmessage = (message) => this.receive(message.data);

      socket.onclose = () => this.scheduleReconnect("connection closed");
      socket.onerror = () => {
        // `onclose` always follows, so the retry is scheduled there.
        this.handlers.onState("connecting", "socket error");
      };
    } catch (error) {
      this.scheduleReconnect(error instanceof Error ? error.message : String(error));
    }
  }

  close(): void {
    this.closed = true;
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.socket?.close();
    this.socket = null;
  }

  subscribe(sessionId: string): void {
    this.subscriptions.add(sessionId);
    this.sendSubscribe(sessionId);
  }

  unsubscribe(sessionId: string): void {
    this.subscriptions.delete(sessionId);
    this.send({ type: "unsubscribe", session_id: sessionId });
  }

  prompt(sessionId: string, prompt: string): void {
    this.send({ type: "prompt", session_id: sessionId, prompt });
  }

  cancel(sessionId: string): void {
    this.send({ type: "cancel", session_id: sessionId });
  }

  answerPermission(
    sessionId: string,
    requestId: string,
    answer: { option_id: string } | { approved: boolean },
  ): void {
    this.send({
      type: "permission_response",
      session_id: sessionId,
      request_id: requestId,
      ...answer,
    });
  }

  listSessions(): void {
    this.send({ type: "list_sessions" });
  }

  createSession(agentId: string, workspace: string): void {
    this.send({ type: "create_session", agent_id: agentId, workspace });
  }

  closeSession(sessionId: string): void {
    this.send({ type: "close_session", session_id: sessionId });
  }

  private sendSubscribe(sessionId: string): void {
    this.send({
      type: "subscribe",
      session_id: sessionId,
      after_seq: loadCursor(sessionId),
    });
  }

  private send(message: Record<string, unknown>): void {
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(JSON.stringify(message));
    }
  }

  private receive(data: unknown): void {
    if (typeof data !== "string") return;
    let frame: unknown;
    try {
      frame = JSON.parse(data);
    } catch {
      return;
    }
    const classified = classify(frame);
    if (classified.kind === "control") {
      this.handlers.onControl(classified.message);
    } else if (classified.kind === "event") {
      // The cursor advances only for events actually handed to the UI, so a
      // reload replays exactly what was not yet rendered.
      saveCursor(classified.event.session_id, classified.event.seq);
      this.handlers.onEvent(classified.event);
    }
  }

  private scheduleReconnect(detail: string): void {
    this.socket = null;
    if (this.closed) return;
    this.handlers.onState("offline", detail);
    const delay = this.backoff;
    this.backoff = Math.min(this.backoff * 2, MAX_BACKOFF_MS);
    this.reconnectTimer = setTimeout(() => void this.connect(), delay) as unknown as number;
  }
}
