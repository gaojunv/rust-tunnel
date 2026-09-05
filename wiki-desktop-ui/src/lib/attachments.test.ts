import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  isAttachmentSrc,
  normalizeAttachmentSrc,
  getAttachmentUrl,
  __clearAttachmentCache,
  __attachmentCacheSize,
} from "./attachments";

vi.mock("@/api/tauri", () => ({
  readAttachment: vi.fn(async (relPath: string) => {
    if (relPath === "assets/missing.png") throw new Error("not found");
    return new Uint8Array([1, 2, 3]);
  }),
}));

import { readAttachment } from "@/api/tauri";

const mockedRead = vi.mocked(readAttachment);

describe("isAttachmentSrc", () => {
  it("accepts assets prefix with and without leading slash", () => {
    expect(isAttachmentSrc("assets/a/b.png")).toBe(true);
    expect(isAttachmentSrc("/assets/a/b.png")).toBe(true);
    expect(isAttachmentSrc("assets")).toBe(true);
    expect(isAttachmentSrc("/assets")).toBe(true);
  });
  it("normalizes backslashes", () => {
    expect(isAttachmentSrc("assets\\a\\b.png")).toBe(true);
  });
  it("rejects non-assets", () => {
    expect(isAttachmentSrc("https://example.com/x.png")).toBe(false);
    expect(isAttachmentSrc("data:image/png;base64,xxx")).toBe(false);
    expect(isAttachmentSrc("blob:http://x")).toBe(false);
    expect(isAttachmentSrc("/other/path.png")).toBe(false);
    expect(isAttachmentSrc(undefined)).toBe(false);
    expect(isAttachmentSrc("")).toBe(false);
    expect(isAttachmentSrc("   ")).toBe(false);
  });
});

describe("normalizeAttachmentSrc", () => {
  it("strips leading slash and trims", () => {
    expect(normalizeAttachmentSrc("/assets/a.png")).toBe("assets/a.png");
    expect(normalizeAttachmentSrc("  /assets/a.png  ")).toBe("assets/a.png");
    expect(normalizeAttachmentSrc("assets/a.png")).toBe("assets/a.png");
  });
  it("normalizes backslashes", () => {
    expect(normalizeAttachmentSrc("\\assets\\a.png")).toBe("assets/a.png");
  });
});

describe("getAttachmentUrl cache + LRU", () => {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let createSpy: any;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let revokeSpy: any;

  beforeEach(() => {
    __clearAttachmentCache();
    mockedRead.mockClear();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    createSpy = vi.spyOn(URL, "createObjectURL").mockImplementation((() => `blob:${Math.random()}`) as any);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    revokeSpy = vi.spyOn(URL, "revokeObjectURL").mockImplementation((() => {}) as any);
  });

  afterEach(() => {
    createSpy.mockRestore();
    revokeSpy.mockRestore();
    __clearAttachmentCache();
  });

  it("hit/miss: second call uses cache without extra read", async () => {
    const a = await getAttachmentUrl("assets/a.png");
    const b = await getAttachmentUrl("assets/a.png");
    expect(a).toBe(b);
    expect(mockedRead).toHaveBeenCalledTimes(1);
    expect(createSpy).toHaveBeenCalledTimes(1);
  });

  it("leading slash and plain path share cache entry", async () => {
    const a = await getAttachmentUrl("/assets/a.png");
    const b = await getAttachmentUrl("assets/a.png");
    expect(a).toBe(b);
    expect(mockedRead).toHaveBeenCalledTimes(1);
  });

  it("LRU eviction revokes oldest blob URL when exceeding cap", async () => {
    // fill to cap + 1
    for (let i = 0; i < 51; i++) {
      await getAttachmentUrl(`assets/img-${i}.png`);
    }
    expect(__attachmentCacheSize()).toBe(50);
    // first inserted img-0 should have been evicted and revoked
    expect(revokeSpy).toHaveBeenCalledTimes(1);
    // img-0 now misses and re-reads
    mockedRead.mockClear();
    await getAttachmentUrl("assets/img-0.png");
    expect(mockedRead).toHaveBeenCalledTimes(1);
  });

  it("concurrent callers coalesce via inflight", async () => {
    let resolve!: (v: Uint8Array) => void;
    mockedRead.mockImplementationOnce(() => new Promise<Uint8Array>((r) => (resolve = r)));
    const p1 = getAttachmentUrl("assets/c.png");
    const p2 = getAttachmentUrl("assets/c.png");
    resolve(new Uint8Array([9]));
    const [a, b] = await Promise.all([p1, p2]);
    expect(a).toBe(b);
    expect(mockedRead).toHaveBeenCalledTimes(1);
  });

  it("read failure does not poison cache", async () => {
    await expect(getAttachmentUrl("assets/missing.png")).rejects.toThrow();
    expect(__attachmentCacheSize()).toBe(0);
    // second attempt should retry
    await expect(getAttachmentUrl("assets/missing.png")).rejects.toThrow();
    expect(mockedRead).toHaveBeenCalledTimes(2);
  });
});
