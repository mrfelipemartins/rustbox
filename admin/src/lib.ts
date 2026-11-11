import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";
import DOMPurify from "dompurify";
export const cn = (...v: ClassValue[]) => twMerge(clsx(v));
// JSON responses contain provider-specific metadata; unknown values are only rendered as text.
export type Row = Record<string, any>;
export type Page = {
  items: Row[];
  total: number;
  page: number;
  per_page: number;
};
export const text = (v: unknown): string =>
  v === null || v === undefined
    ? "—"
    : typeof v === "object"
      ? JSON.stringify(v)
      : String(v);
export const time = (v: unknown) => {
  if (!v) return "—";
  const s = String(v);
  const d = new Date(/Z$|[+-]\d\d:\d\d$/.test(s) ? s : s + "Z");
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString();
};
export const bytes = (v: number) => {
  if (!Number.isFinite(v)) return "—";
  const u = ["B", "KiB", "MiB", "GiB"];
  let i = 0;
  while (v >= 1024 && i < 3) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i ? 1 : 0)} ${u[i]}`;
};
export const mailboxAddress = (r: Row) =>
  r.target_mailbox || r.imap?.username || "—";
export const enc = encodeURIComponent;
export function safeEmailDocument(html: string) {
  const clean = DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true },
    FORBID_TAGS: [
      "style",
      "link",
      "meta",
      "base",
      "form",
      "input",
      "button",
      "video",
      "audio",
      "iframe",
      "object",
      "embed",
      "svg",
      "math",
      "img",
      "picture",
      "source",
    ],
    FORBID_ATTR: [
      "style",
      "src",
      "srcset",
      "href",
      "action",
      "background",
      "poster",
    ],
  });
  return `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; base-uri 'none'"><style>body{font:14px/1.6 system-ui;color:#1f2937;padding:20px;overflow-wrap:anywhere}</style></head><body>${clean}</body></html>`;
}

export function datetimeLocal(value: string) {
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return "";
  const pad = (v: number) => String(v).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
