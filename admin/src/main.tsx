import React, { useEffect, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import {
  BrowserRouter,
  Routes,
  Route,
  NavLink,
  Link,
  useLocation,
  useNavigate,
} from "react-router-dom";
import {
  QueryClient,
  QueryClientProvider,
  useMutation,
} from "@tanstack/react-query";
import {
  Bell,
  Boxes,
  ChevronDown,
  ChevronRight,
  CornerDownLeft,
  Home,
  KeyRound,
  LayoutDashboard,
  LogOut,
  Mail,
  Moon,
  PanelLeft,
  Search,
  Send,
  Settings,
  Sun,
  Terminal,
  Webhook,
} from "lucide-react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import { api, setCsrf, ApiError, useApi, type Session } from "./api";
import { Button } from "./components/ui/button";
import { ErrorBox, Field, Meter, sentence, tone } from "./components/shared";
import { Mailboxes, MailboxDetail } from "./pages/mailboxes";
import { Credentials, OAuthCallback } from "./pages/credentials";
import { Outgoing, SendDetail } from "./pages/outgoing";
import { Events } from "./pages/events";
import { Overview, Logs, System } from "./pages/monitoring";
import { bytes, enc, mailboxAddress, type Page, type Row } from "./lib";
import "./style.css";
const qc = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: true,
      refetchIntervalInBackground: false,
      retry: 1,
    },
  },
});
type Menu = {
  to: string;
  name: string;
  icon: typeof Mail;
  end?: boolean;
  group: "Operate" | "Configure";
};
const menus: Menu[] = [
  { to: "/", name: "Overview", icon: LayoutDashboard, end: true, group: "Operate" },
  { to: "/mailboxes", name: "Mailboxes", icon: Mail, group: "Operate" },
  { to: "/outgoing", name: "Outgoing mail", icon: Send, group: "Operate" },
  { to: "/events", name: "Events & webhooks", icon: Webhook, group: "Operate" },
  { to: "/credentials", name: "Credentials", icon: KeyRound, group: "Configure" },
  { to: "/logs", name: "Logs", icon: Terminal, group: "Configure" },
  { to: "/system", name: "System", icon: Settings, group: "Configure" },
];
function Wordmark() {
  return (
    <span className="wordmark">
      <span className="mark" aria-hidden="true">
        <Boxes size={14} />
      </span>
      rustbox<span className="tag">admin</span>
    </span>
  );
}
function Login({
  onSession,
  error,
}: {
  onSession: (s: Session) => void;
  error?: unknown;
}) {
  const [key, setKey] = useState("");
  const m = useMutation({
    mutationFn: () =>
      api<Session>("/session", {
        method: "POST",
        body: JSON.stringify({ api_key: key }),
      }),
    onSuccess: (s) => {
      setKey("");
      onSession(s);
    },
  });
  return (
    <div className="login-page">
      <Wordmark />
      <main className="login-card frame">
        <div className="panel">
          <h1>Sign in to Rustbox</h1>
          <p>Use an application API key to open an 8-hour admin session.</p>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              m.mutate();
            }}
          >
            <Field
              label="API key"
              hint="Any application key configured for this instance."
            >
              <input
                autoFocus
                required
                type="password"
                autoComplete="off"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                placeholder="rbx_…"
              />
            </Field>
            <ErrorBox error={m.error || error} />
            <Button className="full" disabled={m.isPending} type="submit">
              {m.isPending ? "Signing in…" : "Sign in"}
              <ChevronRight size={16} />
            </Button>
          </form>
        </div>
        <div className="frame-foot">
          Your key is exchanged for a temporary session.
        </div>
      </main>
      <div className="login-footer">
        Self-hosted mail. One place to operate it.
      </div>
    </div>
  );
}
function timeLeft(expires: string) {
  const ms = new Date(expires).getTime() - Date.now();
  if (!Number.isFinite(ms) || ms <= 0) return "expiring";
  const h = Math.floor(ms / 3600000);
  const m = Math.floor((ms % 3600000) / 60000);
  return h ? `${h}h ${m}m left` : `${m}m left`;
}
const dotTone = (state: unknown) =>
  ({ good: "d-success", bad: "d-destructive", warn: "d-warning", pending: "d-info", "": "" })[
    tone(state)
  ];
const textTone = (state: unknown) =>
  ({ good: "t-success", bad: "t-destructive", warn: "t-warning", pending: "t-info", "": "muted" })[
    tone(state)
  ];
function Sidebar({
  session,
  onSearch,
  onLogout,
}: {
  session: Session;
  onSearch: () => void;
  onLogout: () => void;
}) {
  const overview = useApi("/overview", true, 30000).data;
  const boxes = useApi<Page>("/mailboxes?per_page=6", true, 30000).data;
  const [healthOpen, setHealthOpen] = useState(
    () => localStorage.getItem("rustbox-sb-health") !== "closed",
  );
  useEffect(() => {
    localStorage.setItem("rustbox-sb-health", healthOpen ? "open" : "closed");
  }, [healthOpen]);
  const attention: Row[] = overview?.attention || [];
  const byKind = (k: string) => attention.filter((a) => a.kind === k).length;
  const badge: Record<string, ReactNode> = {
    "/mailboxes":
      byKind("mailbox") > 0 ? (
        <span className="pill bad">{byKind("mailbox")}</span>
      ) : boxes ? (
        <span className="count">{boxes.total}</span>
      ) : null,
    "/outgoing":
      byKind("send") + (overview?.sends_unknown || 0) > 0 ? (
        <span className="pill">{Math.max(byKind("send"), overview?.sends_unknown || 0)}</span>
      ) : null,
    "/credentials":
      byKind("credential") > 0 ? <span className="pill">{byKind("credential")}</span> : null,
    "/events":
      overview && overview.webhook_backlog > 0 ? (
        <span className="count">{overview.webhook_backlog}</span>
      ) : null,
  };
  return (
    <>
      <div className="sb-top">
        <Link to="/" aria-label="Rustbox overview">
          <Wordmark />
        </Link>
        <Button
          asChild
          variant="ghost"
          size="icon"
          className="bell"
          aria-label={
            attention.length
              ? `${attention.length} items need attention`
              : "Nothing needs attention"
          }
        >
          <Link to="/">
            <Bell size={16} />
            {attention.length > 0 && <span className="ping" />}
          </Link>
        </Button>
      </div>
      <button className="sb-search" onClick={onSearch}>
        <Search size={15} />
        <span className="grow">Search</span>
        <span className="kbd">⌘K</span>
      </button>
      {(["Operate", "Configure"] as const).map((g) => (
        <nav className="sb-group" key={g} aria-label={g}>
          <div className="nav-label">{g}</div>
          {menus
            .filter((m) => m.group === g)
            .map((m) => (
              <NavLink
                key={m.to}
                to={m.to}
                end={m.end}
                className={({ isActive }) =>
                  isActive ? "nav-item active" : "nav-item"
                }
              >
                <m.icon size={16} />
                {m.name}
                {badge[m.to]}
              </NavLink>
            ))}
        </nav>
      ))}
      {!!boxes?.items.length && (
        <div className="sb-group">
          <button
            className="overline-btn"
            aria-expanded={healthOpen}
            aria-controls="sb-health"
            onClick={() => setHealthOpen(!healthOpen)}
          >
            Mailbox health
            <ChevronDown size={14} />
          </button>
          {healthOpen && (
            <div id="sb-health" className="sb-group">
              {boxes.items.map((b) => (
                <Link className="mbx" key={b.id} to={"/mailboxes/" + enc(b.id)}>
                  <span className="av">
                    {(b.name || mailboxAddress(b)).slice(0, 1)}
                    <span className={`pres ${dotTone(b.state)}`} />
                  </span>
                  <span className="name">{b.name || mailboxAddress(b)}</span>
                  <span className={`st ${textTone(b.state)}`}>
                    {sentence(b.state)}
                  </span>
                </Link>
              ))}
            </div>
          )}
        </div>
      )}
      <div className="sb-card">
        {overview && overview.cache_capacity_bytes > 0 && (
          <Link className="inner" to="/system">
            <div className="head">
              Content cache
              <span className="sub tnum">
                {Math.round((overview.cache_bytes / overview.cache_capacity_bytes) * 100)}%
              </span>
            </div>
            <Meter value={overview.cache_bytes} max={overview.cache_capacity_bytes} />
            <span className="sub">
              {bytes(overview.cache_bytes)} of {bytes(overview.cache_capacity_bytes)}
            </span>
          </Link>
        )}
        <div className="sb-user">
          <span className="av lg">{session.application.slice(0, 1)}</span>
          <div className="who">
            <strong>{session.application}</strong>
            <small>API key session · {timeLeft(session.expires_at)}</small>
          </div>
          <Button size="icon" variant="ghost" aria-label="Sign out" onClick={onLogout}>
            <LogOut size={16} />
          </Button>
        </div>
      </div>
    </>
  );
}
function Palette({
  open,
  onOpenChange,
  actions,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  actions: { name: string; group: string; icon: typeof Mail; run: () => void }[];
}) {
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const list = actions.filter((a) => a.name.toLowerCase().includes(q.toLowerCase()));
  useEffect(() => {
    if (open) {
      setQ("");
      setSel(0);
    }
  }, [open]);
  const run = (i: number) => {
    const a = list[i];
    if (!a) return;
    onOpenChange(false);
    a.run();
  };
  return (
    <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="dialog-overlay" />
        <DialogPrimitive.Content className="dialog-content palette" aria-describedby={undefined}>
          <DialogPrimitive.Title className="sr-only">Search</DialogPrimitive.Title>
          <div className="palette-input">
            <Search size={16} />
            <input
              autoFocus
              aria-label="Search pages and actions"
              placeholder="Jump to a page or action…"
              value={q}
              onChange={(e) => {
                setQ(e.target.value);
                setSel(0);
              }}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setSel((s) => Math.min(list.length - 1, s + 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setSel((s) => Math.max(0, s - 1));
                } else if (e.key === "Enter") {
                  e.preventDefault();
                  run(sel);
                }
              }}
            />
            <span className="kbd">esc</span>
          </div>
          {list.length ? (
            <ul role="listbox" aria-label="Results">
              {list.map((a, i) => (
                <li key={a.name}>
                  <button
                    role="option"
                    aria-selected={i === sel}
                    onMouseEnter={() => setSel(i)}
                    onClick={() => run(i)}
                  >
                    <a.icon size={16} />
                    {a.name}
                    <span className="grp">{a.group}</span>
                    {i === sel && <CornerDownLeft size={14} className="muted" />}
                  </button>
                </li>
              ))}
            </ul>
          ) : (
            <div className="none">No matches for “{q}”.</div>
          )}
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
function App() {
  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [sessionError, setSessionError] = useState<unknown>(null);
  const [mobile, setMobile] = useState(false);
  const [palette, setPalette] = useState(false);
  const [collapsed, setCollapsed] = useState(
    () => localStorage.getItem("rustbox-sidebar") === "collapsed",
  );
  const [dark, setDark] = useState(
    () =>
      localStorage.getItem("rustbox-theme") === "dark" ||
      (!localStorage.getItem("rustbox-theme") &&
        matchMedia("(prefers-color-scheme: dark)").matches),
  );
  const location = useLocation();
  const navigate = useNavigate();
  function accept(s: Session) {
    setCsrf(s.csrf_token);
    setSession(s);
    setSessionError(null);
  }
  useEffect(() => {
    api<Session>("/session")
      .then(accept)
      .catch((e) => {
        if (!(e instanceof ApiError && e.status === 401)) setSessionError(e);
      })
      .finally(() => setLoading(false));
    const expire = () => {
      setSession(null);
      setCsrf("");
      qc.clear();
    };
    window.addEventListener("session-expired", expire);
    return () => window.removeEventListener("session-expired", expire);
  }, []);
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark);
    localStorage.setItem("rustbox-theme", dark ? "dark" : "light");
  }, [dark]);
  useEffect(() => {
    localStorage.setItem("rustbox-sidebar", collapsed ? "collapsed" : "open");
  }, [collapsed]);
  useEffect(() => setMobile(false), [location.pathname]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  async function logout() {
    try {
      await api("/session", { method: "DELETE" });
      setSession(null);
      setCsrf("");
      qc.clear();
    } catch (e) {
      setSessionError(e);
    }
  }
  if (loading)
    return (
      <div className="loading-screen">
        <Boxes className="pulse" size={20} />
        Connecting to Rustbox…
      </div>
    );
  if (!session) return <Login onSession={accept} error={sessionError} />;
  const active = menus.find((m) =>
    m.to === "/"
      ? location.pathname === "/"
      : location.pathname.startsWith(m.to),
  );
  const nested =
    active && active.to !== "/" && location.pathname.length > active.to.length + 1;
  const toggleSidebar = () =>
    matchMedia("(max-width: 860px)").matches
      ? setMobile(!mobile)
      : setCollapsed(!collapsed);
  const actions = [
    ...menus.map((m) => ({
      name: m.name,
      group: m.group,
      icon: m.icon,
      run: () => navigate(m.to),
    })),
    {
      name: dark ? "Switch to light theme" : "Switch to dark theme",
      group: "Preferences",
      icon: dark ? Sun : Moon,
      run: () => setDark(!dark),
    },
    { name: "Sign out", group: "Session", icon: LogOut, run: logout },
  ];
  return (
    <div
      className={
        "app-shell" + (collapsed ? " collapsed" : "") + (mobile ? " mobile-open" : "")
      }
    >
      {mobile && (
        <button
          className="mobile-overlay"
          aria-label="Close navigation"
          onClick={() => setMobile(false)}
        />
      )}
      <aside className="sidebar" aria-label="Sidebar">
        <Sidebar session={session} onSearch={() => setPalette(true)} onLogout={logout} />
      </aside>
      <div className="main-shell">
        <div className="inset">
          <header className="topbar">
            <Button
              size="icon"
              variant="ghost"
              aria-label="Toggle sidebar"
              aria-expanded={mobile || !collapsed}
              onClick={toggleSidebar}
            >
              <PanelLeft size={16} />
            </Button>
            <span className="vsep" />
            <nav aria-label="Breadcrumb">
              <ol className="breadcrumb">
                <li>
                  <Link to="/">
                    <Home size={14} />
                    Home
                  </Link>
                </li>
                {active && active.to !== "/" && (
                  <>
                    <li className="sep" aria-hidden="true">/</li>
                    {nested ? (
                      <li>
                        <Link to={active.to}>{active.name}</Link>
                      </li>
                    ) : (
                      <li className="cur" aria-current="page">{active.name}</li>
                    )}
                  </>
                )}
                {active?.to === "/" && (
                  <>
                    <li className="sep" aria-hidden="true">/</li>
                    <li className="cur" aria-current="page">Overview</li>
                  </>
                )}
                {nested && (
                  <>
                    <li className="sep" aria-hidden="true">/</li>
                    <li className="cur" aria-current="page">Details</li>
                  </>
                )}
              </ol>
            </nav>
            <div className="topbar-end">
              <span className="top-status">
                <i />
                Live
              </span>
              <Button
                variant="ghost"
                size="icon"
                aria-label="Toggle color theme"
                onClick={() => setDark(!dark)}
              >
                {dark ? <Sun size={16} /> : <Moon size={16} />}
              </Button>
            </div>
          </header>
          <main className="page-content">
            <ErrorBox error={sessionError} />
            <Routes>
              <Route path="/" element={<Overview />} />
              <Route path="/mailboxes" element={<Mailboxes />} />
              <Route path="/mailboxes/:id" element={<MailboxDetail />} />
              <Route path="/credentials" element={<Credentials />} />
              <Route path="/oauth/callback" element={<OAuthCallback />} />
              <Route path="/outgoing" element={<Outgoing />} />
              <Route path="/outgoing/:id" element={<SendDetail />} />
              <Route path="/events" element={<Events />} />
              <Route path="/logs" element={<Logs />} />
              <Route path="/system" element={<System />} />
              <Route
                path="*"
                element={
                  <div className="empty">
                    <h1>Page not found</h1>
                    <Link to="/">Return to overview</Link>
                  </div>
                }
              />
            </Routes>
          </main>
          <footer className="app-footer">
            Rustbox <span>Mail infrastructure, under your control.</span>
          </footer>
        </div>
      </div>
      <Palette open={palette} onOpenChange={setPalette} actions={actions} />
    </div>
  );
}
createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={qc}>
      <BrowserRouter basename="/admin">
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </React.StrictMode>,
);
