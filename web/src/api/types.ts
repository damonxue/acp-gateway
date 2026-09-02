// The wire types of the gateway's remote protocol.
//
// These mirror `docs/remote-protocol.md` and the Rust types in
// `crates/gateway-remote/src/protocol.rs`. They are intentionally hand-written
// rather than generated: the protocol is small, stable and documented, and a
// generator would be a build-time dependency for the whole web client.

export type SessionStatus =
  | "idle"
  | "running"
  | "waiting_permission"
  | "cancelling"
  | "completed"
  | "failed"
  | "disconnected";

export interface AgentSession {
  id: string;
  machine_id: string;
  agent_id: string;
  agent_name: string;
  acp_session_id: string | null;
  workspace: string;
  cwd: string;
  title: string | null;
  origin: "gateway" | "ide_bridge";
  status: SessionStatus;
  last_seq: number;
  created_at: string;
  updated_at: string;
}

export interface Machine {
  id: string;
  name: string;
  platform: string;
  hostname: string;
  version: string;
  public_endpoint: string | null;
  created_at: string;
  updated_at: string;
  last_seen_at: string | null;
}

export interface AgentSummary {
  id: string;
  name: string;
}

export interface PermissionOption {
  option_id: string;
  name: string;
  kind: "allow_once" | "allow_always" | "reject_once" | "reject_always";
}

export interface PermissionRequest {
  id: string;
  title: string | null;
  tool_kind: string | null;
  tool_call: unknown;
  options: PermissionOption[];
  requested_at: string;
}

export interface SessionSnapshot extends AgentSession {
  pending_permissions: PermissionRequest[];
  connected: boolean;
  subscribers: number;
}

/** Anything the gateway pushes that is not a control message is an event. */
export interface AgentEvent {
  id: string;
  session_id: string;
  seq: number;
  timestamp: string;
  type: string;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  payload: any;
}

export type ServerMessage =
  | {
      type: "hello";
      protocol_version: number;
      machine: Machine;
      agents: AgentSummary[];
      sessions: AgentSession[];
    }
  | { type: "session_list"; sessions: AgentSession[] }
  | { type: "session_created"; session: AgentSession }
  | {
      type: "subscribed";
      session_id: string;
      last_seq: number;
      snapshot: SessionSnapshot;
    }
  | { type: "error"; code: string; message: string; session_id?: string }
  | { type: "pong" };

/** Control messages carry one of these fixed `type` values. */
const CONTROL_TYPES = new Set([
  "hello",
  "session_list",
  "session_created",
  "subscribed",
  "error",
  "pong",
]);

/**
 * Split an incoming frame into "control message" and "session event".
 *
 * The protocol sends events as themselves (`type` *is* the event type), which
 * means two `type` values are shared with control messages:
 * `session_created` and `error`. So the discriminator is **`seq`**, not `type`:
 * every event carries a sequence number and no control message does. Checking
 * the type first would misread a replayed `session_created` event as the answer
 * to `create_session`.
 */
export function classify(
  frame: unknown,
): { kind: "control"; message: ServerMessage } | { kind: "event"; event: AgentEvent } | { kind: "unknown" } {
  if (typeof frame !== "object" || frame === null) return { kind: "unknown" };
  const candidate = frame as { type?: unknown; seq?: unknown; session_id?: unknown };
  if (typeof candidate.type !== "string") return { kind: "unknown" };
  if (typeof candidate.seq === "number" && typeof candidate.session_id === "string") {
    return { kind: "event", event: frame as AgentEvent };
  }
  if (CONTROL_TYPES.has(candidate.type)) {
    return { kind: "control", message: frame as ServerMessage };
  }
  return { kind: "unknown" };
}

/** The pairing payload encoded in the QR code the desktop shows. */
export interface PairingOffer {
  machine_id: string;
  pairing_code: string;
  nonce: string;
  endpoint?: string;
  machine_public_key: string;
  expires_at: string;
}
