import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  BrowserRouter,
  Routes,
  Route,
  NavLink,
  Link,
  useLocation,
} from "react-router-dom";
import {
  QueryClient,
  QueryClientProvider,
  useMutation,
} from "@tanstack/react-query";
import {
  Activity,
  Boxes,
  ChevronRight,
  KeyRound,
  LayoutDashboard,
  LogOut,
  Mail,
  Menu,
  Moon,
  Send,
  Settings,
  Sun,
  Terminal,
  Webhook,
  X,
} from "lucide-react";
import { api, setCsrf, ApiError, type Session } from "./api";
import { Button } from "./components/ui/button";
import { ErrorBox, Field } from "./components/shared";
import { Mailboxes, MailboxDetail } from "./pages/mailboxes";
import { Credentials, OAuthCallback } from "./pages/credentials";
import { Outgoing, SendDetail } from "./pages/outgoing";
import { Events } from "./pages/events";
import { Overview, Logs, System } from "./pages/monitoring";
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
const menus = [
  { to: "/", name: "Overview", icon: LayoutDashboard, end: true },
  { to: "/mailboxes", name: "Mailboxes", icon: Mail },
  { to: "/credentials", name: "Credentials", icon: KeyRound },
  { to: "/outgoing", name: "Outgoing mail", icon: Send },
  { to: "/events", name: "Events & webhooks", icon: Webhook },
  { to: "/logs", name: "Logs", icon: Terminal },
  { to: "/system", name: "System", icon: Settings },
];
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
      <div className="login-brand">
        <span className="brand-icon">
          <Boxes size={22} />
        </span>
        rustbox<span className="admin-label">ADMIN</span>
      </div>
      <main className="login-card">
        <div className="login-symbol">
          <KeyRound size={24} />
        </div>
        <div className="eyebrow">MAIL INFRASTRUCTURE</div>
        <h1>Welcome to Rustbox</h1>
        <p>Sign in to monitor your service and manage your mailboxes.</p>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            m.mutate();
          }}
        >
          <Field
            label="API key"
            hint="Use any application key configured for this Rustbox instance."
          >
            <input
              autoFocus
              required
              type="password"
              autoComplete="off"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder="Enter your API key"
            />
          </Field>
          <ErrorBox error={m.error || error} />
          <Button className="full" disabled={m.isPending} type="submit">
            {m.isPending ? "Signing in…" : "Sign in"}
            <ChevronRight size={16} />
          </Button>
        </form>
        <small>Your key is exchanged for a temporary session.</small>
      </main>
      <div className="login-footer">
        Self-hosted mail. One place to operate it.
      </div>
    </div>
  );
}
function App() {
  const [session, setSession] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);
  const [sessionError, setSessionError] = useState<unknown>(null);
  const [mobile, setMobile] = useState(false);
  const [dark, setDark] = useState(
    () =>
      localStorage.getItem("rustbox-theme") === "dark" ||
      (!localStorage.getItem("rustbox-theme") &&
        matchMedia("(prefers-color-scheme: dark)").matches),
  );
  const location = useLocation();
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
  useEffect(() => setMobile(false), [location.pathname]);
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
        <Boxes className="pulse" size={32} />
        Connecting to Rustbox…
      </div>
    );
  if (!session) return <Login onSession={accept} error={sessionError} />;
  const active = menus.find((m) =>
    m.to === "/"
      ? location.pathname === "/"
      : location.pathname.startsWith(m.to),
  );
  return (
    <div className="app-shell">
      {mobile && (
        <button
          className="mobile-overlay"
          aria-label="Close navigation"
          onClick={() => setMobile(false)}
        />
      )}
      <aside className={mobile ? "sidebar open" : "sidebar"}>
        <Link to="/" className="brand">
          <span className="brand-icon">
            <Boxes size={22} />
          </span>
          <strong>rustbox</strong>
          <span className="admin-label">ADMIN</span>
        </Link>
        <div className="workspace">
          <span className="workspace-avatar">R</span>
          <div>
            <strong>Rustbox workspace</strong>
            <small>Mail operations</small>
          </div>
          <Activity size={15} />
        </div>
        <div className="nav-label">MANAGE</div>
        <nav>
          {menus.map((m) => (
            <NavLink
              key={m.to}
              to={m.to}
              end={m.end}
              className={({ isActive }) =>
                isActive ? "nav-item active" : "nav-item"
              }
            >
              <m.icon size={18} />
              {m.name}
            </NavLink>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="environment">
            <i />
            SELF-HOSTED INSTANCE
          </div>
          <div className="session">
            <span className="session-avatar">
              {session.application.slice(0, 1).toUpperCase()}
            </span>
            <div>
              <strong>{session.application}</strong>
              <small>Administrator</small>
            </div>
            <Button
              size="icon"
              variant="ghost"
              aria-label="Sign out"
              onClick={logout}
            >
              <LogOut size={16} />
            </Button>
          </div>
        </div>
      </aside>
      <div className="main-shell">
        <header className="topbar">
          <div className="breadcrumb">
            <Button
              size="icon"
              variant="ghost"
              className="mobile-menu"
              aria-label={mobile ? "Close navigation" : "Open navigation"}
              onClick={() => setMobile(!mobile)}
            >
              {mobile ? <X size={18} /> : <Menu size={18} />}
            </Button>
            <span>Workspace</span>
            <ChevronRight size={13} />
            <strong>{active?.name || "Administration"}</strong>
          </div>
          <div className="actions">
            <span className="top-status">
              <i />
              Rustbox admin
            </span>
            <Button
              variant="ghost"
              size="icon"
              aria-label="Toggle color theme"
              onClick={() => setDark(!dark)}
            >
              {dark ? <Sun size={17} /> : <Moon size={17} />}
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
