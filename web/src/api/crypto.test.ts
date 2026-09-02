import { verifyAsync } from "@noble/ed25519";
import { describe, expect, it } from "vitest";
import {
  fromBase64,
  generateKeypair,
  signBase64,
  ticketChallenge,
  toBase64,
} from "./crypto";
import { parseOffer, socketUrl } from "./gateway";

describe("device identity", () => {
  it("produces a 32-byte key the gateway can verify", async () => {
    const keypair = await generateKeypair();
    expect(fromBase64(keypair.publicKeyBase64)).toHaveLength(32);

    const challenge = ticketChallenge("machine_1", "device_1", 1_780_000_000_000);
    expect(challenge).toBe("ws-ticket:machine_1:device_1:1780000000000");

    const signature = await signBase64(keypair.privateKey, challenge);
    const verified = await verifyAsync(
      fromBase64(signature),
      new TextEncoder().encode(challenge),
      fromBase64(keypair.publicKeyBase64),
    );
    expect(verified).toBe(true);
  });

  it("round-trips base64", () => {
    const bytes = new Uint8Array([0, 1, 2, 250, 255]);
    expect(fromBase64(toBase64(bytes))).toEqual(bytes);
  });
});

describe("pairing payload", () => {
  it("accepts the documented QR JSON", () => {
    const offer = parseOffer(
      JSON.stringify({
        machine_id: "machine_1",
        pairing_code: "834921",
        nonce: "abc",
        endpoint: "https://gw.example.com",
        machine_public_key: "key",
        expires_at: new Date().toISOString(),
      }),
    );
    expect(offer.pairing_code).toBe("834921");
  });

  it("rejects anything else", () => {
    expect(() => parseOffer("hello")).toThrow();
    expect(() => parseOffer('{"machine_id":"m"}')).toThrow(/pairing code/);
  });
});

describe("socket url", () => {
  it("upgrades the scheme and carries the ticket", () => {
    expect(socketUrl("https://gw.example.com", "tkt_1")).toBe(
      "wss://gw.example.com/remote?ticket=tkt_1",
    );
    expect(socketUrl("http://127.0.0.1:48100", "tkt/2")).toBe(
      "ws://127.0.0.1:48100/remote?ticket=tkt%2F2",
    );
  });
});
