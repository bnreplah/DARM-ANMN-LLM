// IndexedDB-backed key/value store. Holds the non-extractable CryptoKeys by
// structured clone, so the private key never exists as bytes in JavaScript.

const DB = 'darm-companion';
const TABLE = 'kv';

function open() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(TABLE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function tx(mode, fn) {
  const db = await open();
  return new Promise((resolve, reject) => {
    const t = db.transaction(TABLE, mode);
    const request = fn(t.objectStore(TABLE));
    t.oncomplete = () => {
      db.close();
      resolve(request.result);
    };
    t.onerror = () => reject(t.error);
  });
}

export const idbStore = {
  get: (key) => tx('readonly', (s) => s.get(key)),
  set: (key, value) => tx('readwrite', (s) => s.put(value, key)),
  delete: (key) => tx('readwrite', (s) => s.delete(key)),
};
