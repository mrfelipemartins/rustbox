import {
  cloneElement,
  isValidElement,
  useId,
  useState,
  type ReactNode,
  type ReactElement,
} from "react";
import { useSearchParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import {
  AlertCircle,
  ChevronLeft,
  ChevronRight,
  Loader2,
  RefreshCw,
  Inbox,
} from "lucide-react";
import { Button } from "./ui/button";
import { Modal } from "./ui/dialog";
import { text, time, type Row, type Page } from "../lib";
export function Heading({
  eyebrow,
  title,
  description,
  children,
}: {
  eyebrow?: string;
  title: string;
  description: string;
  children?: ReactNode;
}) {
  return (
    <div className="page-heading">
      <div>
        <div className="eyebrow">{eyebrow || "WORKSPACE"}</div>
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      <div className="actions">{children}</div>
    </div>
  );
}
export function Badge({ value }: { value: unknown }) {
  const s = text(value);
  return (
    <span
      className={`badge ${/^(ready|connected|healthy|accepted|delivered|complete|completed|matched|available)$/.test(s) ? "good" : /fail|unknown|expired|error|denied|reauthor/i.test(s) ? "bad" : /pending|queue|connecting|prepar|syncing/.test(s) ? "pending" : ""}`}
    >
      <i />
      {s.replaceAll("_", " ")}
    </span>
  );
}
export function ErrorBox({ error }: { error: unknown }) {
  return error ? (
    <div className="error" role="alert">
      <AlertCircle size={17} />
      {error instanceof Error ? error.message : text(error)}
    </div>
  ) : null;
}
export function LoadState({
  loading,
  error,
  empty = false,
  children,
}: {
  loading: boolean;
  error: unknown;
  empty?: boolean;
  children: ReactNode;
}) {
  if (loading)
    return (
      <div className="empty" role="status">
        <Loader2 className="spin" />
        Loading…
      </div>
    );
  if (error) return <ErrorBox error={error} />;
  if (empty)
    return (
      <div className="empty">
        <Inbox size={28} />
        <strong>No records found</strong>
        <span>Records will appear here when activity is available.</span>
      </div>
    );
  return children;
}
export function Json({ value }: { value: unknown }) {
  return <pre className="json">{JSON.stringify(value, null, 2)}</pre>;
}
export function Detail({ value }: { value: Row }) {
  return (
    <dl className="details">
      {Object.entries(value)
        .filter(([, v]) => v !== undefined)
        .map(([k, v]) => (
          <div key={k}>
            <dt>{k.replaceAll("_", " ")}</dt>
            <dd>
              {typeof v === "object" && v !== null ? (
                <Json value={v} />
              ) : k.endsWith("_at") || k === "available_until" ? (
                time(v)
              ) : (
                text(v)
              )}
            </dd>
          </div>
        ))}
    </dl>
  );
}
export function Inspect({ title, value }: { title: string; value: unknown }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button variant="ghost" size="sm" onClick={() => setOpen(true)}>
        Inspect
      </Button>
      <Modal title={title} open={open} onOpenChange={setOpen}>
        <Json value={value} />
      </Modal>
    </>
  );
}
export function useFilters() {
  const [p, setP] = useSearchParams();
  const set = (key: string, value: string) => {
    const n = new URLSearchParams(p);
    value ? n.set(key, value) : n.delete(key);
    if (key !== "page") n.delete("page");
    setP(n, { replace: true });
  };
  return {
    params: p,
    set,
    query: (extra: Record<string, string> = {}) => {
      const n = new URLSearchParams(p);
      n.delete("tab");
      Object.entries(extra).forEach(([k, v]) =>
        v ? n.set(k, v) : n.delete(k),
      );
      return n.toString();
    },
  };
}
export function Filters({
  children,
  search = true,
}: {
  children?: ReactNode;
  search?: boolean;
}) {
  const { params, set } = useFilters();
  const qc = useQueryClient();
  return (
    <div className="filters">
      {search && (
        <input
          aria-label="Search records"
          placeholder="Search…"
          value={params.get("q") || ""}
          onChange={(e) => set("q", e.target.value)}
        />
      )}
      {children}
      <Button
        variant="outline"
        size="icon"
        aria-label="Refresh"
        onClick={() => qc.invalidateQueries()}
      >
        <RefreshCw size={15} />
      </Button>
    </div>
  );
}
export function FilterSelect({
  name,
  label,
  options,
}: {
  name: string;
  label: string;
  options: string[];
}) {
  const { params, set } = useFilters();
  return (
    <select
      aria-label={label}
      value={params.get(name) || ""}
      onChange={(e) => set(name, e.target.value)}
    >
      <option value="">{label}</option>
      {options.map((v) => (
        <option key={v}>{v}</option>
      ))}
    </select>
  );
}
export function DateFilters() {
  const { params, set } = useFilters();
  return (
    <>
      {["from", "to"].map((k) => (
        <label className="date-filter" key={k}>
          {k}
          <input
            type="date"
            aria-label={`${k} date (UTC)`}
            value={(params.get(k) || "").slice(0, 10)}
            onChange={(e) =>
              set(
                k,
                e.target.value
                  ? e.target.value +
                      (k === "to" ? "T23:59:59.999Z" : "T00:00:00Z")
                  : "",
              )
            }
          />
        </label>
      ))}
    </>
  );
}
export function Pagination({ data }: { data?: Page }) {
  const { set } = useFilters();
  if (!data) return null;
  const page = data.page || 1;
  return (
    <div className="pagination">
      <span>
        {data.total} records · Page {page} of{" "}
        {Math.max(1, Math.ceil(data.total / (data.per_page || 50)))}
      </span>
      <div>
        <Button
          variant="outline"
          size="sm"
          disabled={page <= 1}
          onClick={() => set("page", String(page - 1))}
        >
          <ChevronLeft size={14} />
          Previous
        </Button>
        <Button
          variant="outline"
          size="sm"
          disabled={page * (data.per_page || 50) >= data.total}
          onClick={() => set("page", String(page + 1))}
        >
          Next
          <ChevronRight size={14} />
        </Button>
      </div>
    </div>
  );
}
export function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: ReactNode;
  hint?: string;
}) {
  const id = useId();
  const control = isValidElement(children)
    ? cloneElement(
        children as ReactElement<{
          "aria-labelledby"?: string;
          "aria-describedby"?: string;
        }>,
        {
          "aria-labelledby": id,
          "aria-describedby": hint ? `${id}-hint` : undefined,
        },
      )
    : children;
  return (
    <label className="field">
      <span id={id}>{label}</span>
      {control}
      {hint && <small id={`${id}-hint`}>{hint}</small>}
    </label>
  );
}
export function Table({
  headers,
  children,
}: {
  headers: string[];
  children: ReactNode;
}) {
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            {headers.map((h) => (
              <th key={h}>{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}
export function Section({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="panel">
      <h2>{title}</h2>
      {children}
    </section>
  );
}
