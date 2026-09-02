// End-to-end smoke test of the phone path, without a browser.
//
// It performs exactly what the web client does — pair, sign a ticket challenge,
// open the socket, drive a session — using the same @noble/ed25519 the bundle
// ships. That makes it the check that matters most: Ed25519 signatures produced
// by JavaScript must verify in the Rust gateway, and the WebSocket protocol must
// behave as documented.
//
// Usage:
//   node scripts/smoke.mjs [base-url] [agent-id]

import { getPublicKeyAsync, signAsync } from "@noble/ed25519";

const base = process.argv[2] ?? "http://127.0.0.1:48125";
const agentId = process.argv[3] ?? "fake";

const toBase64 = (bytes) => Buffer.from(bytes).toString("base64");

async function postJson(path, body) {
  const response = await fetch(base + path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`${path} → ${response.status}: ${text}`);
  return JSON.parse(text);
}

function step(message) {
  console.log(`✓ ${message}`);
}

async function main() {
  // 1. The desktop mints a pairing code (the app or `agent-gateway pair`).
  const offer = await postJson("/pairing/begin", {});
  step(`pairing code ${offer.pairing_code} for ${offer.machine_id}`);

  // 2. The phone generates its keypair and redeems the code.
  const privateKey = crypto.getRandomValues(new Uint8Array(32));
  const publicKey = await getPublicKeyAsync(privateKey);
  const device = await postJson("/pairing/consume", {
    pairing_code: offer.pairing_code,
    nonce: offer.nonce,
    device_name: "Smoke Test",
    platform: "web",
    public_key: toBase64(publicKey),
  });
  step(`paired as ${device.id}`);

  // 3. A signed request buys one single-use ticket.
  const issuedAt = new Date();
  const challenge = `ws-ticket:${offer.machine_id}:${device.id}:${issuedAt.getTime()}`;
  const signature = toBase64(await signAsync(new TextEncoder().encode(challenge), privateKey));
  const ticket = await postJson(`/devices/${device.id}/ws-ticket`, {
    issued_at: issuedAt.toISOString(),
    signature,
  });
  step(`ticket ${ticket.id} (js signature verified by the gateway)`);

  // 4. Open the socket and drive a session end to end.
  const socket = new WebSocket(
    `${base.replace(/^http/, "ws")}/remote?ticket=${encodeURIComponent(ticket.id)}`,
  );
  const seen = [];
  let sessionId = null;

  const finished = new Promise((resolve, reject) => {
    const failure = setTimeout(() => reject(new Error("timed out")), 30_000);

    socket.onopen = () => step("socket open");
    socket.onerror = () => reject(new Error("socket error"));
    socket.onmessage = (message) => {
      const frame = JSON.parse(message.data);

      // A frame with a `seq` is an event, whatever its `type` says: the event
      // types `session_created` and `error` are also control message names.
      if (typeof frame.seq === "number") {
        seen.push(frame.type);
        if (frame.type === "permission_request") {
          step(`permission requested: ${frame.payload.title}`);
          socket.send(
            JSON.stringify({
              type: "permission_response",
              session_id: sessionId,
              request_id: frame.payload.id,
              option_id: frame.payload.options[0].option_id,
            }),
          );
        }
        if (frame.type === "session_completed") {
          clearTimeout(failure);
          resolve();
        }
        return;
      }

      switch (frame.type) {
        case "hello":
          step(`hello from ${frame.machine.name}, ${frame.agents.length} agent(s)`);
          socket.send(
            JSON.stringify({
              type: "create_session",
              agent_id: agentId,
              workspace: "/tmp",
            }),
          );
          break;
        case "session_created":
          sessionId = frame.session.id;
          step(`session ${sessionId}`);
          socket.send(JSON.stringify({ type: "subscribe", session_id: sessionId }));
          break;
        case "subscribed":
          step(`subscribed at seq ${frame.last_seq}`);
          socket.send(
            JSON.stringify({
              type: "prompt",
              session_id: sessionId,
              prompt: "needs permission",
            }),
          );
          break;
        case "error":
          clearTimeout(failure);
          reject(new Error(`gateway error: ${frame.code} ${frame.message}`));
          break;
        default:
          break;
      }
    };
  });

  await finished;
  socket.close();

  const required = [
    "session_created",
    "user_message",
    "permission_request",
    "permission_response",
    "agent_message_chunk",
    "session_completed",
  ];
  const missing = required.filter((type) => !seen.includes(type));
  if (missing.length > 0) {
    throw new Error(`missing events: ${missing.join(", ")}`);
  }
  step(`received ${seen.length} events: ${[...new Set(seen)].join(", ")}`);

  // 5. A ticket is single-use: the same one must not open a second socket.
  const replay = new WebSocket(
    `${base.replace(/^http/, "ws")}/remote?ticket=${encodeURIComponent(ticket.id)}`,
  );
  await new Promise((resolve, reject) => {
    replay.onopen = () => reject(new Error("a used ticket was accepted twice"));
    replay.onerror = () => resolve();
    replay.onclose = () => resolve();
  });
  step("a used ticket is refused");

  console.log("\nphone path works end to end");
}

main().catch((error) => {
  console.error(`✗ ${error.message}`);
  process.exit(1);
});
