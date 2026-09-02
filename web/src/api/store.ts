// Local persistence: the device credential and per-session replay cursors.
//
// The credential lives in IndexedDB rather than localStorage: it holds a private
// key, and localStorage is both string-only and cleared more eagerly by mobile
// browsers. Replay cursors are disposable, so they stay in localStorage.

export interface StoredDevice {
  deviceId: string;
  deviceName: string;
  machineId: string;
  machineName: string;
  /** Base URL of the gateway, when it differs from the page's origin. */
  endpoint: string | null;
  privateKey: Uint8Array;
  publicKeyBase64: string;
  pairedAt: string;
}

const DB_NAME = "agent-gateway";
const DB_VERSION = 1;
const STORE = "device";
const DEVICE_KEY = "current";
const CURSOR_PREFIX = "agent-gateway.cursor.";

function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function transact<T>(
  mode: IDBTransactionMode,
  run: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  const db = await openDatabase();
  try {
    return await new Promise<T>((resolve, reject) => {
      const request = run(db.transaction(STORE, mode).objectStore(STORE));
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
  } finally {
    db.close();
  }
}

export async function loadDevice(): Promise<StoredDevice | null> {
  try {
    const stored = await transact<StoredDevice | undefined>("readonly", (store) =>
      store.get(DEVICE_KEY),
    );
    return stored ?? null;
  } catch {
    // A browser in private mode may refuse IndexedDB entirely; the app then
    // behaves as "not paired" rather than crashing.
    return null;
  }
}

export async function saveDevice(device: StoredDevice): Promise<void> {
  await transact("readwrite", (store) => store.put(device, DEVICE_KEY));
}

export async function forgetDevice(): Promise<void> {
  await transact("readwrite", (store) => store.delete(DEVICE_KEY));
  for (const key of Object.keys(localStorage)) {
    if (key.startsWith(CURSOR_PREFIX)) localStorage.removeItem(key);
  }
}

/** Highest sequence number this device has already rendered for a session. */
export function loadCursor(sessionId: string): number {
  const raw = localStorage.getItem(CURSOR_PREFIX + sessionId);
  const parsed = raw === null ? 0 : Number.parseInt(raw, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : 0;
}

export function saveCursor(sessionId: string, seq: number): void {
  if (seq > loadCursor(sessionId)) {
    localStorage.setItem(CURSOR_PREFIX + sessionId, String(seq));
  }
}
