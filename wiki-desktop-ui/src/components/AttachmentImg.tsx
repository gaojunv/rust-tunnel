import { useEffect, useState } from "react";
import type { ExtraProps } from "streamdown";
import { getAttachmentUrl, isAttachmentSrc, normalizeAttachmentSrc } from "@/lib/attachments";

type Props = React.ImgHTMLAttributes<HTMLImageElement> & ExtraProps & {
  src?: string;
  alt?: string;
};

export function AttachmentImg({ src: rawSrc, alt, className, ...rest }: Props) {
  const src = typeof rawSrc === "string" ? rawSrc : undefined;
  const isAtt = isAttachmentSrc(src);
  const [blobUrl, setBlobUrl] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!isAtt || !src) return;
    let cancelled = false;
    setBlobUrl(null);
    setErr(null);
    const rel = normalizeAttachmentSrc(src);
    getAttachmentUrl(rel)
      .then((url) => {
        if (!cancelled) setBlobUrl(url);
      })
      .catch((e: unknown) => {
        if (!cancelled) setErr(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [isAtt, src]);

  const domRest = ((): React.ImgHTMLAttributes<HTMLImageElement> => {
    const { node: _n, ...r } = rest as Record<string, unknown> & { node?: unknown };
    void _n;
    return r as React.ImgHTMLAttributes<HTMLImageElement>;
  })();
  if (!isAtt) {
    return <img src={src} alt={alt ?? ""} className={`max-w-full rounded ${className ?? ""}`.trim()} {...domRest} />;
  }

  if (err) {
    return (
      <span className="inline-flex max-w-full items-center gap-1 rounded border border-destructive/30 bg-destructive/10 px-2 py-1 text-xs text-destructive">
        图片加载失败{alt ? `：${alt}` : ""} — {err}
      </span>
    );
  }

  if (!blobUrl) {
    return (
      <span className="inline-flex h-6 max-w-full items-center rounded bg-muted px-2 text-xs text-muted-foreground">
        加载中…{alt ? ` ${alt}` : ""}
      </span>
    );
  }

  return <img src={blobUrl} alt={alt ?? ""} className={`max-w-full rounded ${className ?? ""}`.trim()} {...domRest} />;
}
