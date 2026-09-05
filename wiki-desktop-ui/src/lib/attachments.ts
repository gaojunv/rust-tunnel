import { readAttachment } from "@/api/tauri";

const MAX_ENTRIES = 50;

const cache = new Map<string, string>();
const inflight = new Map<string, Promise<string>>();

function guessMime(relPath: string): string {
  const ext = relPath.split(".").pop()?.toLowerCase() ?? "";
  switch (ext) {
    case "png":
      return "image/png";
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "gif":
      return "image/gif";
    case "webp":
      return "image/webp";
    case "svg":
      return "image/svg+xml";
    case "bmp":
      return "image/bmp";
    case "avif":
      return "image/avif";
    default:
      return "application/octet-stream";
  }
}

function ensureLruLimit(): void {
  while (cache.size > MAX_ENTRIES) {
    const oldest = cache.keys().next().value as string | undefined;
    if (!oldest) break;
    const url = cache.get(oldest)!;
    cache.delete(oldest);
    try {
      URL.revokeObjectURL(url);
    } catch {
      // ignore
    }
  }
}

export function isAttachmentSrc(src: string | undefined): boolean {
  if (!src) return false;
  const s = src.trim();
  if (!s) return false;
  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(s)) return false;
  if (s.startsWith("blob:")) return false;
  const norm = s.replace(/\\/g, "/").replace(/^\/+/, "");
  return norm === "assets" || norm.startsWith("assets/");
}

export function normalizeAttachmentSrc(src: string): string {
  return src.trim().replace(/\\/g, "/").replace(/^\/+/, "");
}

export async function getAttachmentUrl(relPath: string): Promise<string> {
  const key = normalizeAttachmentSrc(relPath);
  const cached = cache.get(key);
  if (cached) {
    cache.delete(key);
    cache.set(key, cached);
    return cached;
  }
  const pending = inflight.get(key);
  if (pending) return pending;
  const p = (async () => {
    const bytes = await readAttachment(key);
    const mime = guessMime(key);
    const blob = new Blob([bytes as unknown as BlobPart], { type: mime });
    const url = URL.createObjectURL(blob);
    cache.set(key, url);
    ensureLruLimit();
    return url;
  })();
  inflight.set(key, p);
  try {
    return await p;
  } finally {
    inflight.delete(key);
  }
}

/** Test helper: clear cache and revoke all blob URLs */
export function __clearAttachmentCache(): void {
  for (const url of cache.values()) {
    try {
      URL.revokeObjectURL(url);
    } catch {
      // ignore
    }
  }
  cache.clear();
  inflight.clear();
}

/** Test helper: current cache size */
export function __attachmentCacheSize(): number {
  return cache.size;
}
