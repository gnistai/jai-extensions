// Tiny IndexedDB key-value store in the extension origin. Used for the API
// token (never chrome.storage, which content scripts can read), the render
// cache and the local render history.
const DB = 'jai-viewer';
const STORES = ['secrets', 'cache', 'history'] as const;
type Store = (typeof STORES)[number];

let dbp: Promise<IDBDatabase> | null = null;
function db(): Promise<IDBDatabase> {
  dbp ??= new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => { for (const s of STORES) if (!req.result.objectStoreNames.contains(s)) req.result.createObjectStore(s); };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  return dbp;
}

function tx<T>(store: Store, mode: IDBTransactionMode, f: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return db().then(d => new Promise<T>((resolve, reject) => {
    const r = f(d.transaction(store, mode).objectStore(store));
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  }));
}

export const idbGet = <T>(store: Store, key: string): Promise<T | undefined> => tx<T | undefined>(store, 'readonly', s => s.get(key) as IDBRequest<T | undefined>);
export const idbSet = (store: Store, key: string, value: unknown): Promise<IDBValidKey> => tx(store, 'readwrite', s => s.put(value, key));
export const idbDelete = (store: Store, key: string): Promise<undefined> => tx(store, 'readwrite', s => s.delete(key));
export const idbClear = (store: Store): Promise<undefined> => tx(store, 'readwrite', s => s.clear());
export const idbAll = <T>(store: Store): Promise<T[]> => tx<T[]>(store, 'readonly', s => s.getAll() as IDBRequest<T[]>);
