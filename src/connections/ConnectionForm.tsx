import { useState } from "react";
import * as api from "../api/tauri";
import type { ConnectionProfile, DbKind, Environment, NetworkProfile, ProfileBase, SslMode } from "../api/types";
import { Button } from "../components/Button";
import { useShortcuts } from "../hooks/useShortcuts";
import { useApp } from "../state/store";
import { capabilities } from "./profile";

export const newProfile = (): ConnectionProfile => ({
  id: "",
  name: "",
  kind: "postgres",
  transport: "network",
  host: "localhost",
  port: 5432,
  user: "",
  database: "",
  defaultSchema: null,
  environment: "local",
  sslMode: "prefer",
  sslRootCert: null,
  connectTimeoutSecs: 10,
  readOnly: false,
  startupSql: null,
});

const ENVIRONMENTS: { value: Environment; label: string }[] = [
  { value: "local", label: "Local" },
  { value: "staging", label: "Staging" },
  { value: "production", label: "Production" },
];
const SSL_MODES: SslMode[] = ["disable", "prefer", "require", "verify-ca", "verify-full"];
/** MongoDB has no opportunistic TLS, so no `prefer`; `disable` is a new profile's default. */
const MONGODB_SSL_MODES: SslMode[] = ["disable", "require", "verify-ca", "verify-full"];
const STATEMENT_LIMIT_ERROR = "The statement time limit must be a whole number of seconds.";
/** The largest limit the profile holds (a u32 in Rust). */
const MAX_STATEMENT_LIMIT = 4_294_967_295;
const MONGODB_READ_ONLY_NOTE =
  "MongoDB has no read-only sessions: the app refuses saves. For a hard block, connect as a user with the read role.";
const TYPES: { kind: DbKind; badge: string; label: string; sub: string; enabled: boolean }[] = [
  { kind: "postgres", badge: "PG", label: "PostgreSQL", sub: "Server", enabled: true },
  { kind: "mysql", badge: "MY", label: "MySQL / MariaDB", sub: "Server", enabled: true },
  { kind: "sqlite", badge: "SQ", label: "SQLite", sub: "File", enabled: true },
  { kind: "mongodb", badge: "MG", label: "MongoDB", sub: "Server", enabled: true },
  { kind: "sqlserver", badge: "MS", label: "SQL Server", sub: "Later", enabled: false },
];

/**
 * The profile as another database type: the name, environment, read-only mode and startup SQL
 * carry over; SQLite starts with no file, PostgreSQL with today's server defaults. MongoDB has no
 * startup SQL and starts with TLS off (`prefer` means nothing there). Only a new
 * connection picks its type: a saved one's cards are disabled, so a saved PostgreSQL profile never
 * turns into SQLite in place (its Keychain password would stay behind).
 */
function withKind(p: ConnectionProfile, kind: DbKind): ConnectionProfile {
  if (p.kind === kind) return p;
  const kept = { id: p.id, name: p.name, environment: p.environment, readOnly: p.readOnly, startupSql: p.startupSql ?? null };
  const caps = capabilities(kind);
  if (caps.transport === "file") {
    return { ...kept, kind, transport: "file", filePath: "", defaultSchema: null, connectTimeoutSecs: newProfile().connectTimeoutSecs };
  }
  const fresh = newProfile();
  const profile: ConnectionProfile = { ...fresh, ...kept, kind, ...(fresh.transport === "network" && caps.defaultPort !== null ? { port: caps.defaultPort } : {}) };
  return kind === "mongodb" && profile.transport === "network" ? { ...profile, sslMode: "disable", startupSql: null } : profile;
}

/** Inline validation message, or null when the profile can be used. */
function validate(p: ConnectionProfile): string | null {
  if (p.transport === "network" && (!Number.isInteger(p.port) || p.port < 1 || p.port > 65535)) return "Port must be a whole number from 1 to 65535";
  if (!Number.isInteger(p.connectTimeoutSecs) || p.connectTimeoutSecs < 1) return "Timeout must be a whole number of seconds, at least 1";
  const limit = p.statementTimeoutSecs;
  if (p.kind === "mongodb" && limit !== undefined && (!Number.isInteger(limit) || limit < 1 || limit > MAX_STATEMENT_LIMIT)) return STATEMENT_LIMIT_ERROR;
  return null;
}

/** Checked when the user tests, saves or connects, so an empty new form doesn't start with an error. */
const missing = (p: ConnectionProfile): string | null =>
  p.transport === "file" && p.filePath.trim() === "" ? "Choose a database file." : null;

const errorMessage = (e: unknown) => (e as { message?: string })?.message ?? String(e);

type Status = { kind: "ok" | "error"; text: string } | null;

interface Props {
  initial: ConnectionProfile;
  onSaved: (saved: ConnectionProfile) => Promise<void>;
  onDeleted: (id: string) => Promise<void>;
}

export function ConnectionForm({ initial, onSaved, onDeleted }: Props) {
  const connectTo = useApp((s) => s.connectTo);
  const activeId = useApp((s) => s.active?.profile?.id);
  const [p, setP] = useState(initial);
  const [password, setPassword] = useState("");
  const [passwordTouched, setPasswordTouched] = useState(false);
  const [status, setStatus] = useState<Status>(null);
  const [busy, setBusy] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const isSaved = p.id !== "";
  const isActive = isSaved && activeId === p.id;
  const invalid = validate(p);

  const set = <K extends keyof ProfileBase>(key: K, value: ProfileBase[K]) =>
    setP((cur) => ({ ...cur, [key]: value }));
  const setNet = <K extends keyof NetworkProfile>(key: K, value: NetworkProfile[K]) =>
    setP((cur) => (cur.transport === "network" ? { ...cur, [key]: value } : cur));
  const setFile = (filePath: string) => {
    setP((cur) => (cur.transport === "file" ? { ...cur, filePath } : cur));
    setStatus(null);
  };
  /** MongoDB's Auth database; the key is left out of the profile while it is empty. */
  const setAuthSource = (value: string) =>
    setP((cur) => {
      if (cur.transport !== "network") return cur;
      const { authSource: _old, ...rest } = cur;
      return value === "" ? rest : { ...rest, authSource: value };
    });
  /**
   * MongoDB's statement time limit; the key is left out of the profile while the field is empty.
   * Text the browser can't read as a number (`1.`, `-`, letters) is NaN, which `validate` refuses.
   */
  const setStatementLimit = (input: HTMLInputElement) => {
    const { value } = input;
    const unreadable = input.validity.badInput;
    setP((cur) => {
      if (cur.transport !== "network") return cur;
      const { statementTimeoutSecs: _old, ...rest } = cur;
      if (value === "" && !unreadable) return rest;
      return { ...rest, statementTimeoutSecs: value === "" ? NaN : Number(value) };
    });
  };
  const text = (v: string | null | undefined) => v ?? "";
  const orNull = (v: string) => (v === "" ? null : v);

  const run = async (fn: () => Promise<Status | void>, validated = true) => {
    const problem = invalid ?? missing(p);
    if (validated && problem) {
      setStatus({ kind: "error", text: problem });
      return;
    }
    setBusy(true);
    setStatus(null);
    try {
      setStatus((await fn()) ?? null);
    } catch (e) {
      setStatus({ kind: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  const mongodb = p.kind === "mongodb";
  // A file has no password.
  const network = p.transport === "network";
  const savePassword = network && passwordTouched ? password : undefined;
  const testPassword = network && (!isSaved || passwordTouched) ? password : undefined;

  const save = async () => {
    const result = await api.saveConnection(p, savePassword);
    setP((cur) => ({ ...cur, id: result.id }));
    await onSaved(result);
    return result;
  };

  const test = () =>
    run(async () => {
      const r = await api.testConnection(p, testPassword);
      return { kind: "ok", text: `✓ Test passed · ${r.latencyMs} ms` };
    });
  const saveOnly = () => run(async () => void (await save()));
  const connect = () =>
    run(async () => {
      const saved = await save();
      await connectTo(saved.id);
    });
  // Two-step delete: window.confirm() is unreliable in the Tauri webview.
  const remove = () => {
    setConfirmingDelete(false);
    return run(async () => {
      await api.deleteConnection(p.id);
      await onDeleted(p.id);
    }, false);
  };

  useShortcuts({ "mod+enter": () => void (confirmingDelete || busy || invalid || p.transport === "uri" || connect()) });

  const choose = async () => {
    try {
      const path = await api.pickDatabaseFile();
      if (path !== null) setFile(path);
    } catch (e) {
      setStatus({ kind: "error", text: errorMessage(e) });
    }
  };

  // URI connections can't be edited until their form exists.
  if (p.transport === "uri") {
    return (
      <form className="conn-form" onSubmit={(e) => e.preventDefault()}>
        <div className="conn-form-kicker">Edit connection</div>
        <h2>{p.name}</h2>
        <p>This connection type can't be edited yet.</p>
      </form>
    );
  }

  return (
    <form className="conn-form" onSubmit={(e) => e.preventDefault()}>
      <header className="conn-form-head">
        <div>
          <div className="conn-form-kicker">{isSaved ? "Edit connection" : "New connection"}</div>
          <h2>{p.name}</h2>
        </div>
        {isActive && (
          <span className="conn-pill">
            <span className="conn-pill-dot" aria-hidden="true" />
            Connected
          </span>
        )}
      </header>

      <div className="conn-field">
        <div className="conn-field-label">Database type</div>
        <div className="conn-types" role="radiogroup" aria-label="Database type">
          {TYPES.map((t) => (
            <button
              key={t.label}
              type="button"
              role="radio"
              aria-checked={p.kind === t.kind}
              aria-label={`${t.label} ${t.sub}`}
              disabled={!t.enabled || isSaved}
              className="conn-type"
              onClick={() => {
                setP((cur) => withKind(cur, t.kind));
                setStatus(null);
              }}
            >
              <span className="conn-badge" aria-hidden="true">{t.badge}</span>
              <span className="conn-type-text">
                <strong>{t.label}</strong>
                <span>{t.sub}</span>
              </span>
            </button>
          ))}
        </div>
      </div>

      <label>Name<input value={p.name} onChange={(e) => set("name", e.target.value)} /></label>
      {p.transport === "file" ? (
        <div className="conn-file">
          <label>
            Database file
            <input value={p.filePath} placeholder="/path/to/database.sqlite" onChange={(e) => setFile(e.target.value)} />
          </label>
          <Button variant="secondary" label="Choose…" onClick={() => void choose()} disabled={busy} />
        </div>
      ) : (
        <>
          <div className="conn-pair">
            <label>Host<input value={p.host} onChange={(e) => setNet("host", e.target.value)} /></label>
            <label className="conn-port">
              Port
              <input type="number" value={p.port} onChange={(e) => setNet("port", Number(e.target.value))} />
            </label>
          </div>
          <div className="conn-pair">
            <label>User<input value={p.user} onChange={(e) => setNet("user", e.target.value)} /></label>
            <label>
              Password
              <input
                type="password"
                value={password}
                placeholder={isSaved ? "Stored in Keychain" : ""}
                onChange={(e) => {
                  setPassword(e.target.value);
                  setPasswordTouched(true);
                }}
              />
            </label>
          </div>
          <div className="conn-pair">
            <label>
              {capabilities(p.kind).schemaLabel === "Database" ? "Database (optional)" : "Database"}
              <input value={p.database} onChange={(e) => setNet("database", e.target.value)} />
            </label>
            {mongodb && (
              <label>
                Auth database (optional)
                <input value={text(p.authSource)} placeholder="admin" onChange={(e) => setAuthSource(e.target.value)} />
              </label>
            )}
            {capabilities(p.kind).schemaField && (
              <label>
                Schema (optional)
                <input
                  value={text(p.defaultSchema)}
                  placeholder="public"
                  onChange={(e) => set("defaultSchema", orNull(e.target.value))}
                />
              </label>
            )}
          </div>
        </>
      )}

      <div className="conn-field">
        <div className="conn-field-label">Environment tag</div>
        <div role="radiogroup" aria-label="Environment" className="conn-envs">
          {ENVIRONMENTS.map((env) => (
            <button
              key={env.value}
              type="button"
              role="radio"
              aria-checked={p.environment === env.value}
              className="conn-env"
              data-env={env.value}
              onClick={() => set("environment", env.value)}
            >
              <span className="conn-dot" data-env={env.value} aria-hidden="true" />
              {env.label}
            </button>
          ))}
        </div>
      </div>

      <div className="conn-disclosures">
        {p.transport === "network" && (
          <>
            <details>
              <summary>
                SSL / TLS <span className="conn-summary-value">{p.sslMode}</span>
              </summary>
              <div className="conn-details-body">
                <label>
                  SSL mode
                  <select value={p.sslMode} onChange={(e) => setNet("sslMode", e.target.value as SslMode)}>
                    {(mongodb ? MONGODB_SSL_MODES : SSL_MODES).map((m) => (
                      <option key={m} value={m}>{m}</option>
                    ))}
                  </select>
                </label>
                <label>
                  CA file path
                  <input value={text(p.sslRootCert)} onChange={(e) => setNet("sslRootCert", orNull(e.target.value))} />
                </label>
              </div>
            </details>

            <details>
              <summary>
                SSH tunnel <span className="conn-summary-value">Coming later</span>
              </summary>
            </details>
          </>
        )}

        <details>
          <summary>
            {mongodb
              ? "Advanced: timeout, read-only mode"
              : p.transport === "network"
                ? "Advanced: timeout, read-only mode, startup SQL"
                : "Advanced: read-only mode, startup SQL"}
          </summary>
          <div className="conn-details-body">
            {p.transport === "network" && (
              <label>
                Timeout (seconds)
                <input
                  type="number"
                  value={p.connectTimeoutSecs}
                  onChange={(e) => set("connectTimeoutSecs", Number(e.target.value))}
                />
              </label>
            )}
            {mongodb && (
              <label>
                Statement time limit (seconds)
                <input
                  type="number"
                  value={p.statementTimeoutSecs === undefined || Number.isNaN(p.statementTimeoutSecs) ? "" : p.statementTimeoutSecs}
                  placeholder="None"
                  onChange={(e) => setStatementLimit(e.target)}
                />
              </label>
            )}
            <label className="conn-check">
              <input type="checkbox" checked={p.readOnly} onChange={(e) => set("readOnly", e.target.checked)} />
              Read-only
            </label>
            {mongodb ? (
              <p className="conn-note">{MONGODB_READ_ONLY_NOTE}</p>
            ) : (
              <label>
                Startup SQL
                <textarea value={text(p.startupSql)} onChange={(e) => set("startupSql", orNull(e.target.value))} />
              </label>
            )}
          </div>
        </details>
      </div>

      <footer className="conn-actions">
        {confirmingDelete ? (
          <>
            <span className="conn-confirm">Delete “{p.name}”?</span>
            <Button variant="primary" danger label="Delete" onClick={remove} disabled={busy} />
            <Button variant="secondary" label="Cancel" onClick={() => setConfirmingDelete(false)} disabled={busy} />
          </>
        ) : (
          <>
            {isSaved && (
              <Button
                variant="ghost"
                danger
                label="Delete"
                onClick={() => {
                  setStatus(null);
                  setConfirmingDelete(true);
                }}
                disabled={busy}
              />
            )}
            {invalid && !status && <span className="conn-status" data-kind="error">✗ {invalid}</span>}
            {status && (
              <span className="conn-status" data-kind={status.kind} role="status">
                {status.kind === "error" && !status.text.startsWith("✗") ? `✗ ${status.text}` : status.text}
              </span>
            )}
            <Button variant="secondary" label="Test connection" onClick={test} disabled={busy || !!invalid} />
            <Button variant="secondary" label="Save" onClick={saveOnly} disabled={busy || !!invalid} />
            <Button variant="primary" label="Connect" shortcut="⌘↵" onClick={connect} disabled={busy || !!invalid} />
          </>
        )}
      </footer>
    </form>
  );
}
