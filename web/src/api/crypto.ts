// Device identity: an Ed25519 keypair generated in the browser.
//
// The private key never leaves the device — it is what proves "this phone is the
// one that was paired" when asking for a WebSocket ticket. WebCrypto only
// supports Ed25519 on Safari 17+ / Chrome 137+, and a phone on iOS 16 must
// still be able to pair, so we use @noble/ed25519 (same algorithm, pure JS).

import { getPublicKeyAsync, signAsync } from "@noble/ed25519";

/** Standard, padded base64 — the encoding the gateway expects. */
export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

export function fromBase64(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

export interface DeviceKeypair {
  privateKey: Uint8Array;
  publicKeyBase64: string;
}

/** Generate a fresh device keypair from the platform CSPRNG. */
export async function generateKeypair(): Promise<DeviceKeypair> {
  const privateKey = new Uint8Array(32);
  crypto.getRandomValues(privateKey);
  const publicKey = await getPublicKeyAsync(privateKey);
  return { privateKey, publicKeyBase64: toBase64(publicKey) };
}

/**
 * The exact bytes the gateway expects a device to sign when asking for a
 * ticket. Must stay identical to `AuthService::ticket_challenge`.
 */
export function ticketChallenge(
  machineId: string,
  deviceId: string,
  issuedAtMillis: number,
): string {
  return `ws-ticket:${machineId}:${deviceId}:${issuedAtMillis}`;
}

export async function signBase64(
  privateKey: Uint8Array,
  message: string,
): Promise<string> {
  const signature = await signAsync(new TextEncoder().encode(message), privateKey);
  return toBase64(signature);
}
