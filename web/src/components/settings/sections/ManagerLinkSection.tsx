import { useCallback, useEffect, useState } from "react"
import { LanIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { InfoButton } from "@/components/ui/InfoButton"
import { Toggle } from "@/components/ui/Toggle"
import { groupFingerprint } from "./managerLink"

/** GET /api/manager-link */
export interface ManagerLinkStatus {
  enabled: boolean
  address: string
  default_address: string | null
  effective_address: string | null
  web_password_set: boolean
  paired: boolean
  pairing: boolean
  pairing_seconds_left: number
  fingerprint: string | null
  fingerprint_display: string | null
  state:
    | "off" | "needs_address" | "needs_password" | "update_needed" | "cert_changed"
    | "pairing" | "pairing_expired" | "connected" | "not_paired" | "connecting" | "retrying"
  message: string
  connected: boolean
  last_error: string | null
  last_connected: number | null
}

const STATE_LABEL: Record<ManagerLinkStatus["state"], { label: string; tone: string }> = {
  off: { label: "Off", tone: "text-slate-400" },
  needs_address: { label: "Not set up", tone: "text-amber-300" },
  needs_password: { label: "Not set up", tone: "text-amber-300" },
  update_needed: { label: "Update needed", tone: "text-amber-300" },
  cert_changed: { label: "Certificate changed", tone: "text-rose-300" },
  pairing: { label: "Waiting to pair", tone: "text-blue-300" },
  pairing_expired: { label: "Not paired", tone: "text-amber-300" },
  connected: { label: "Connected", tone: "text-emerald-300" },
  not_paired: { label: "Not paired", tone: "text-amber-300" },
  connecting: { label: "Connecting…", tone: "text-blue-300" },
  retrying: { label: "Paired · offline", tone: "text-slate-300" },
}

/** Where the web password is set (Setup Wizard → Security). */
export const WEB_PASSWORD_LINK = "/settings?tab=System&wizard=security"

/** One line of the Pi's security log for the link (GET /api/manager-link/log). */
export interface LinkLogEntry {
  at: number
  level: "info" | "warn"
  kind: string
  msg: string
  address?: string
  seen?: string
  expected?: string | null
  method?: string
  path?: string
  status?: number
  ssid?: string | null
  gw?: string | null
  error?: string
  reason?: string
}

interface LinkLog { entries: LinkLogEntry[]; warnings: LinkLogEntry[] }


function describe(e: LinkLogEntry): string {
  const bits: string[] = [e.msg]
  if (e.method && e.path) bits.push(`${e.method} ${e.path}${e.status ? ` → ${e.status}` : ""}`)
  if (e.kind === "network") bits.push([e.ssid ? `Wi-Fi ${e.ssid}` : null, e.gw ? `gateway ${e.gw}` : null].filter(Boolean).join(", "))
  if (e.error) bits.push(e.error)
  if (e.reason) bits.push(e.reason)
  if (e.address && !e.method) bits.push(e.address)
  return bits.filter(Boolean).join(" · ")
}

function isLog(v: unknown): v is LinkLog {
  return !!v && typeof v === "object" && Array.isArray((v as LinkLog).entries) && Array.isArray((v as LinkLog).warnings)
}

function isStatus(v: unknown): v is ManagerLinkStatus {
  return !!v && typeof v === "object" && typeof (v as ManagerLinkStatus).state === "string"
    && typeof (v as ManagerLinkStatus).enabled === "boolean"
}

function ago(unix: number | null): string | null {
  if (!unix) return null
  const s = Math.max(0, Math.round(Date.now() / 1000 - unix))
  if (s < 60) return "just now"
  if (s < 3600) return `${Math.round(s / 60)} min ago`
  if (s < 86400) return `${Math.round(s / 3600)} h ago`
  return new Date(unix * 1000).toLocaleString()
}

/** Settings → System → Sentry Manager. */
export function ManagerLinkSection() {
  const [status, setStatus] = useState<ManagerLinkStatus | null>(null)
  // What the user is typing; null shows the saved address.
  const [draft, setDraft] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmUnpair, setConfirmUnpair] = useState(false)
  const [log, setLog] = useState<LinkLog | null>(null)
  const [showLog, setShowLog] = useState(false)

  const apply = useCallback((data: unknown) => {
    if (!isStatus(data)) throw new Error("Could not read the Sentry Manager status.")
    setStatus(data)
  }, [])

  useEffect(() => {
    let active = true
    async function poll() {
      try {
        const r = await fetch("/api/manager-link", { cache: "no-store" })
        if (!r.ok) throw new Error()
        const data = await r.json()
        if (active) { apply(data); setError(e => e === "Could not load the Sentry Manager status." ? null : e) }
      } catch {
        if (active) setError("Could not load the Sentry Manager status.")
      }
    }
    void poll()
    // Status changes on its own (pairing, reconnects), so keep it fresh.
    const id = setInterval(poll, 3000)
    return () => { active = false; clearInterval(id) }
  }, [apply])

  useEffect(() => {
    let active = true
    async function load() {
      try {
        const r = await fetch("/api/manager-link/log", { cache: "no-store" })
        if (!r.ok) return
        const data = await r.json()
        if (active && isLog(data)) setLog(data)
      } catch { /* the status line already reports a failure */ }
    }
    void load()
    const id = setInterval(load, 10000)
    return () => { active = false; clearInterval(id) }
  }, [])

  useEffect(() => {
    if (!confirmUnpair) return
    const id = setTimeout(() => setConfirmUnpair(false), 8000)
    return () => clearTimeout(id)
  }, [confirmUnpair])

  async function call(method: "PUT" | "POST", path: string, body?: unknown) {
    setBusy(true)
    setError(null)
    try {
      const r = await fetch(path, {
        method,
        headers: body === undefined ? undefined : { "Content-Type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
      })
      const data = await r.json().catch(() => ({}))
      if (!r.ok) throw new Error(data?.error || "That didn't work. Try again.")
      apply(data)
      return true
    } catch (e) {
      setError(e instanceof Error ? e.message : "That didn't work. Try again.")
      return false
    } finally {
      setBusy(false)
    }
  }

  const s = status
  const label = s ? STATE_LABEL[s.state] ?? STATE_LABEL.connecting : null
  const address = draft ?? s?.address ?? ""
  const addressChanged = !!s && draft !== null && draft.trim() !== s.address
  const lastSeen = s && !s.connected ? ago(s.last_connected) : null

  return (
    <PrefCard
      icon={<LanIcon className="h-3.5 w-3.5" />}
      halo="blue"
      title="Sentry Manager"
      badge={label && <span className={"text-xs font-medium " + label.tone}>{label.label}</span>}
      help={<InfoButton title="Sentry Manager">
        <p>Sentry Manager runs on your NAS. When this is on, this Sentry USB connects out to Manager whenever it's on your home network, so you can see its status, change settings, read logs and install updates from Manager.</p>
        <p>Nothing on this Sentry USB is opened to the network. The link is encrypted, and both sides prove who they are on every connection.</p>
        <p>Pairing uses this Sentry USB's web username and password once. Manager never stores the password; afterwards both sides use a random key.</p>
        <p>Over the link, Manager can't open a terminal, read or change files, see passwords, change the web login, resize drives, or switch updates to another source.</p>
      </InfoButton>}
    >
      <Toggle
        label="Managed by Sentry Manager"
        checked={!!s?.enabled}
        disabled={!s || busy}
        onChange={on => void call("PUT", "/api/manager-link", { enabled: on, address: s?.address ?? "" })}
        sub={s ? (s.enabled ? "On" : "Off") : error ? "Unavailable" : "Loading…"}
      />

      {s && s.enabled && <>
        <p role="status" className="text-sm text-slate-300">{s.message}</p>

        {s.state === "needs_password" && (
          <a href={WEB_PASSWORD_LINK} className="self-start rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25">
            Set a web password
          </a>
        )}

        {s.state === "retrying" && s.last_error && (
          <p className="t-xs">Last error: {s.last_error}{lastSeen ? ` · last connected ${lastSeen}` : ""}</p>
        )}

        <div className="flex flex-col gap-1">
          <label htmlFor="manager-address" className="t-md">Manager address</label>
          <div className="flex items-center gap-2">
            <input
              id="manager-address"
              type="text"
              inputMode="url"
              spellCheck={false}
              value={address}
              disabled={busy}
              placeholder={s.default_address ?? "nas.local:10000"}
              onChange={e => setDraft(e.target.value)}
              className="min-w-0 flex-1 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 font-mono text-xs text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25 disabled:opacity-50"
            />
            {addressChanged && (
              <button type="button" disabled={busy}
                onClick={() => void call("PUT", "/api/manager-link", { enabled: s.enabled, address: address.trim() }).then(ok => { if (ok) setDraft(null) })}
                className="shrink-0 rounded-lg bg-blue-500/15 px-3 py-1.5 text-[11px] font-medium text-blue-300 hover:bg-blue-500/25 disabled:opacity-50">
                Save
              </button>
            )}
          </div>
          <p className="t-xs">
            {s.address
              ? "Leave empty to use your archive server's address."
              : s.default_address
                ? `Automatic: ${s.default_address} (your archive server, port 10000).`
                : "Enter your NAS's address, for example nas.local:10000."}
          </p>
        </div>

        {s.fingerprint_display && (
          <div className="flex flex-col gap-1">
            <span className="t-md">Manager's fingerprint</span>
            <code className="break-words rounded-lg border border-white/10 bg-white/5 px-3 py-2 font-mono text-xs leading-relaxed text-slate-200">
              {s.fingerprint_display}
            </code>
            <p className="t-xs">Check that Sentry Manager shows the same fingerprint for this Sentry USB.</p>
          </div>
        )}

        <div className="flex flex-wrap gap-2">
          {s.web_password_set && !s.pairing && (s.state === "pairing_expired" || s.state === "not_paired" || s.state === "retrying" || s.state === "connected") && (
            <button type="button" disabled={busy} onClick={() => void call("POST", "/api/manager-link/pair")}
              className="rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25 disabled:opacity-50">
              {s.paired ? "Pair again" : "Pair"}
            </button>
          )}
          {(s.paired || s.fingerprint) && (
            <button type="button" disabled={busy}
              onClick={() => {
                if (!confirmUnpair) { setConfirmUnpair(true); return }
                setConfirmUnpair(false)
                void call("POST", "/api/manager-link/unpair")
              }}
              className="rounded-lg border border-rose-500/30 bg-rose-500/10 px-3 py-1.5 text-xs font-medium text-rose-300 transition-colors hover:bg-rose-500/20 disabled:opacity-50">
              {confirmUnpair ? "Confirm unpair" : "Unpair"}
            </button>
          )}
        </div>
      </>}

      {log && log.warnings.length > 0 && (
        <div role="alert" className="flex flex-col gap-1.5 rounded-lg border border-rose-500/40 bg-rose-500/10 p-3">
          <p className="text-sm font-semibold text-rose-200">Possible fake Sentry Manager</p>
          {log.warnings.map((w, i) => (
            <div key={i} className="flex flex-col gap-0.5">
              <p className="text-xs text-rose-100">
                {new Date(w.at * 1000).toLocaleString()}: {w.kind === "fingerprint_mismatch"
                  ? `a server at ${w.address ?? "Manager's address"} presented a different certificate.`
                  : `a server at ${w.address ?? "Manager's address"} couldn't prove it is your Manager.`}
              </p>
              {w.seen && <code className="break-words font-mono text-[11px] text-rose-200">Seen: {groupFingerprint(w.seen)}</code>}
            </div>
          ))}
          <p className="text-xs text-rose-100">Nothing was sent to it. If you didn't reinstall or replace Manager, someone on that network may be impersonating it.</p>
        </div>
      )}

      {log && log.entries.length > 0 && (
        <div className="flex flex-col gap-1.5">
          <button type="button" onClick={() => setShowLog(v => !v)} aria-expanded={showLog}
            className="self-start text-xs font-medium text-blue-400 hover:text-blue-300">
            {showLog ? "Hide security log" : "Show security log"}
          </button>
          {showLog && (
            <ul className="max-h-72 overflow-y-auto rounded-lg border border-white/10 bg-white/5 text-xs">
              {log.entries.slice(0, 100).map((e, i) => (
                <li key={i} className={"border-b border-white/5 px-3 py-1.5 last:border-0 " + (e.level === "warn" ? "text-rose-300" : "text-slate-300")}>
                  <span className="mr-2 text-slate-500">{new Date(e.at * 1000).toLocaleString()}</span>
                  <span className="break-words">{describe(e)}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      {s && !s.web_password_set && (
        <p className="t-xs">
          A web password must be set for pairing.{" "}
          {s.enabled ? null : <a href={WEB_PASSWORD_LINK} className="text-blue-400 underline">Set one in the Setup Wizard.</a>}
        </p>
      )}

      {error && <p role="alert" className="text-xs text-rose-300">{error}</p>}
    </PrefCard>
  )
}
