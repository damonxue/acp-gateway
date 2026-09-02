// The session list: what the PRD's home screen describes — machine, then its
// sessions with live status.

import { useState } from "preact/hooks";
import type { StoredDevice } from "../api/store";
import type { AgentSession, AgentSummary, Machine } from "../api/types";

interface Props {
  machine: Machine | null;
  device: StoredDevice;
  agents: AgentSummary[];
  sessions: AgentSession[];
  onSelect(sessionId: string): void;
  onRefresh(): void;
  onCreate(agentId: string, workspace: string): void;
  onForget(): Promise<void>;
}

export function Sessions({
  machine,
  device,
  agents,
  sessions,
  onSelect,
  onRefresh,
  onCreate,
  onForget,
}: Props) {
  const [creating, setCreating] = useState(false);
  const [agentId, setAgentId] = useState(agents[0]?.id ?? "");
  const [workspace, setWorkspace] = useState("");

  return (
    <div class="sessions">
      <header>
        <div>
          <h1>{machine?.name ?? device.machineName}</h1>
          <p class="muted small">
            {machine ? `${machine.platform} · ${machine.hostname}` : device.machineId}
          </p>
        </div>
        <button type="button" class="secondary" onClick={onRefresh}>
          Refresh
        </button>
      </header>

      <ul class="list">
        {sessions.length === 0 && (
          <li class="muted">No sessions yet. Start one below.</li>
        )}
        {sessions.map((session) => (
          <li key={session.id}>
            <button type="button" class="card" onClick={() => onSelect(session.id)}>
              <div class="row">
                <strong>{session.title ?? session.agent_name}</strong>
                <Status status={session.status} />
              </div>
              <div class="muted small mono">{session.workspace}</div>
            </button>
          </li>
        ))}
      </ul>

      {creating ? (
        <div class="card form">
          <label>
            Agent
            <select
              value={agentId}
              onChange={(event) => setAgentId((event.target as HTMLSelectElement).value)}
            >
              {agents.map((agent) => (
                <option key={agent.id} value={agent.id}>
                  {agent.name}
                </option>
              ))}
            </select>
          </label>
          <label>
            Workspace (absolute path on the computer)
            <input
              value={workspace}
              spellcheck={false}
              placeholder="/Users/you/project"
              onInput={(event) => setWorkspace((event.target as HTMLInputElement).value)}
            />
          </label>
          <div class="row gap">
            <button
              type="button"
              disabled={agentId === "" || workspace.trim() === ""}
              onClick={() => {
                onCreate(agentId, workspace.trim());
                setCreating(false);
              }}
            >
              Start
            </button>
            <button type="button" class="secondary" onClick={() => setCreating(false)}>
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          disabled={agents.length === 0}
          onClick={() => {
            setAgentId(agents[0]?.id ?? "");
            setCreating(true);
          }}
        >
          New session
        </button>
      )}

      <footer>
        <span class="muted small">Paired as {device.deviceName}</span>
        <button type="button" class="link" onClick={() => void onForget()}>
          Unpair this device
        </button>
      </footer>
    </div>
  );
}

export function Status({ status }: { status: string }) {
  return <span class={`status ${status}`}>{status.replace(/_/g, " ")}</span>;
}
