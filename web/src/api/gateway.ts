// HTTP calls: pairing and ticket issuance.
//
// These are the only two authenticated HTTP endpoints a remote client uses.
// Everything else happens over the WebSocket, which is the gateway's deliberate
// boundary: one remote-facing session protocol, not two.

import { generateKeypair, signBase64, ticketChallenge } from "./crypto";
import { saveDevice, type StoredDevice } from "./store";
import type { PairingOffer } from "./types";

/**
 * Where the gateway is.
 *
 * The bundle is served *by* the gateway, so the page's own origin is the right
 * answer in both deployments (`127.0.0.1:48100/app` and `gw.example.com/app`).
 * A stored endpoint overrides it, which covers hosting the client elsewhere.
 */
export function apiBase(device?: StoredDevice | null): string {
  return device?.endpoint ?? location.origin;
}

export function socketUrl(base: string, ticket: string): string {
  const url = new URL(base);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.pathname = "/remote";
  url.search = `?ticket=${encodeURIComponent(ticket)}`;
  return url.toString();
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) {
    // The gateway's error body is `{code, message}`; show the message.
    let message = text;
    try {
      const parsed = JSON.parse(text) as { message?: string };
      if (parsed.message) message = parsed.message;
    } catch {
      /* keep the raw body */
    }
    throw new Error(message || `${response.status} ${response.statusText}`);
  }
  return JSON.parse(text) as T;
}

/** Parse the JSON carried by the pairing QR code (or pasted by hand). */
export function parseOffer(raw: string): PairingOffer {
  const offer = JSON.parse(raw.trim()) as PairingOffer;
  if (!offer.machine_id || !offer.pairing_code || !offer.nonce) {
    throw new Error("this does not look like a pairing code");
  }
  return offer;
}

/**
 * Redeem a pairing code: generate a keypair, register it, store the credential.
 *
 * The pairing code is single-use, so a failure here means the user has to ask
 * the desktop for a new one — which is the intended cost of a stolen code.
 */
export async function pair(
  offer: PairingOffer,
  deviceName: string,
): Promise<StoredDevice> {
  const keypair = await generateKeypair();
  const base = offer.endpoint ?? location.origin;
  const registered = await postJson<{ id: string; name: string }>(
    `${base}/pairing/consume`,
    {
      pairing_code: offer.pairing_code,
      nonce: offer.nonce,
      device_name: deviceName,
      platform: platformName(),
      public_key: keypair.publicKeyBase64,
    },
  );

  const device: StoredDevice = {
    deviceId: registered.id,
    deviceName,
    machineId: offer.machine_id,
    machineName: offer.machine_id,
    endpoint: offer.endpoint && offer.endpoint !== location.origin ? offer.endpoint : null,
    privateKey: keypair.privateKey,
    publicKeyBase64: keypair.publicKeyBase64,
    pairedAt: new Date().toISOString(),
  };
  await saveDevice(device);
  return device;
}

/**
 * Obtain a single-use WebSocket ticket by signing a fresh challenge.
 *
 * A ticket is good for exactly one connection, so this runs before every
 * connect and every reconnect.
 */
export async function fetchTicket(device: StoredDevice): Promise<string> {
  const issuedAt = new Date();
  const challenge = ticketChallenge(
    device.machineId,
    device.deviceId,
    issuedAt.getTime(),
  );
  const signature = await signBase64(device.privateKey, challenge);
  const ticket = await postJson<{ id: string }>(
    `${apiBase(device)}/devices/${encodeURIComponent(device.deviceId)}/ws-ticket`,
    { issued_at: issuedAt.toISOString(), signature },
  );
  return ticket.id;
}

function platformName(): string {
  const agent = navigator.userAgent;
  if (/iPhone|iPad|iPod/.test(agent)) return "ios";
  if (/Android/.test(agent)) return "android";
  return "web";
}
