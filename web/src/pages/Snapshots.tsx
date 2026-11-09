import { SelectMenu } from "@/components/ui/SelectMenu"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { CheckCircleIcon, DeleteIcon, ErrorIcon, HardDriveIcon, ProgressActivityIcon, ScheduleIcon } from "@/components/icons"
import { Modal } from "@/components/ui/Modal"
import type { ManagedStorageHealth } from "@/lib/api"
import { responseError } from "@/lib/file-upload"

/** How much of a snapshot's footage is verified (SHA-256) on the archive
 *  server. Absent when verification is off or there is no archive server. */
export interface ArchiveCoverage {
  state: "verified" | "pending" | "partial" | "failed" | "unverified" | "nothing"
  clips: number
  verified: number
  verified_twice: number
  /** Of verified: re-encoded in place by Sentry Manager (checked by codec, not checksum). */
  converted?: number
  /** Of verified: matched by the SMB/NFS quick check rather than a full checksum. */
  quick?: number
  /** Re-encoded by Sentry Manager before anything confirmed the original. */
  unconfirmed?: number
  pending: number
  failed: number
  not_archived: number
  ready_to_delete: boolean
  last_checked_unix: number | null
}
interface SnapshotEntry {
  id: string
  // Reclaim is cumulative: this snapshot and every older holder of shared blocks.
  cumulative_reclaim_bytes: number | null
  older_count: number
  created_unix: number
  archive?: ArchiveCoverage | null
}
interface FreeSpace {
  total_bytes: number
  used_bytes: number
  available_bytes: number
  mounted: boolean
  storage_health?: ManagedStorageHealth
}
interface Deletion {
  entries: SnapshotEntry[]
  estimate: number | null
  through: boolean
}
const buttonClass =
  "inline-flex min-h-9 items-center justify-center gap-2 rounded-xl border border-white/10 px-3 py-2 text-sm text-slate-300 hover:bg-white/5 disabled:opacity-50"
function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B"
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), 3)
  return `${(bytes / 1024 ** unit).toFixed(unit ? 1 : 0)} ${["B", "KB", "MB", "GB"][unit]}`
}
function formatDate(unix: number) {
  return unix ? new Date(unix * 1000).toLocaleString() : "Unknown date"
}

const tagClass = "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px] font-medium"

/** "Verified on archive" and related tags for one snapshot row. */
/** GET /api/snapshots/live */
interface LiveStatus {
  car_last_write_ts: number | null
  next_scheduled_ts: number | null
  server_ts: number
}
/** GET /api/clips/freshness, POST /api/clips/refresh (the app's contract). */
interface Freshness {
  snapshot_ts: number | null
  refreshing: boolean
  next_refresh_ts: number | null
  checked_ts: number | null
  server_ts: number
}
/** The car counts as recording if it wrote to its drive this recently. */
const RECORDING_WITHIN_SECS = 120

function formatAgo(secs: number): string {
  if (secs < 60) return `${Math.max(0, Math.round(secs))} s`
  if (secs < 3600) return `${Math.round(secs / 60)} min`
  const h = Math.floor(secs / 3600)
  const m = Math.round((secs % 3600) / 60)
  return m ? `${h} h ${m} min` : `${h} h`
}

/** The top row: footage the car has recorded that isn't in a snapshot yet.
 *  The car's drive is never read directly while the car writes to it (that
 *  could see half-updated folders); "Snapshot now" takes a real snapshot the
 *  same safe way the scheduled ones are taken, and the new clips then show
 *  up as a new snapshot row. */
export function LiveRow({ onSnapshotTaken }: { onSnapshotTaken: () => void }) {
  const [live, setLive] = useState<LiveStatus | null>(null)
  const [fresh, setFresh] = useState<Freshness | null>(null)
  const [note, setNote] = useState<string | null>(null)
  const wasRefreshing = useRef(false)
  const load = useCallback(async () => {
    try {
      const [l, f] = await Promise.all([fetch("/api/snapshots/live"), fetch("/api/clips/freshness")])
      if (l.ok) setLive(await l.json())
      if (f.ok) {
        const next: Freshness = await f.json()
        setFresh(next)
        // A snapshot started here (or by the app) just finished.
        if (wasRefreshing.current && !next.refreshing) onSnapshotTaken()
        wasRefreshing.current = next.refreshing
      }
    } catch {
      // Offline for a moment: keep the last values.
    }
  }, [onSnapshotTaken])
  // Poll every 30 s, every 5 s while a snapshot is being taken; the first
  // load is scheduled like the others rather than run inside the effect.
  useEffect(() => {
    const refreshing = fresh?.refreshing === true
    const first = setTimeout(() => void load(), 0)
    const timer = setInterval(() => {
      if (!document.hidden) void load()
    }, refreshing ? 5000 : 30000)
    return () => {
      clearTimeout(first)
      clearInterval(timer)
    }
  }, [load, fresh?.refreshing])
  async function snapshotNow() {
    setNote(null)
    try {
      const response = await fetch("/api/clips/refresh", { method: "POST" })
      const next: Freshness = await response.json()
      setFresh(next)
      wasRefreshing.current = next.refreshing
      if (!next.refreshing)
        setNote(next.next_refresh_ts && next.next_refresh_ts > next.server_ts
          ? `A snapshot was taken recently; try again in ${formatAgo(next.next_refresh_ts - next.server_ts)}.`
          : "Can't take a snapshot right now (an archive is running, a snapshot is already in progress, or space is low).")
    } catch {
      setNote("Couldn't reach the Pi. Try again.")
    }
  }
  const now = live?.server_ts ?? fresh?.server_ts ?? 0
  const carAge = live?.car_last_write_ts != null ? now - live.car_last_write_ts : null
  const recording = carAge !== null && carAge <= RECORDING_WITHIN_SECS
  const snapAge = fresh?.snapshot_ts != null ? now - fresh.snapshot_ts : null
  const nextIn = live?.next_scheduled_ts != null ? live.next_scheduled_ts - now : null
  const waitFor = fresh?.next_refresh_ts != null && fresh.next_refresh_ts > now ? fresh.next_refresh_ts - now : null
  return (
    <section aria-label="Live" className="glass-card flex flex-wrap items-center justify-between gap-3 border-l-2 border-l-red-400 px-4 py-3">
      <div className="min-w-48 flex-1">
        <p className="flex flex-wrap items-center gap-2 text-sm font-medium text-slate-200">
          <span className={`${tagClass} ${recording ? "border-red-400/40 bg-red-500/15 text-red-300" : "border-white/10 bg-white/5 text-slate-400"}`}>
            <span className={`h-1.5 w-1.5 rounded-full ${recording ? "animate-pulse bg-red-400" : "bg-slate-500"}`} />
            {recording ? "Live · recording" : "Live · not recording"}
          </span>
          Footage not in a snapshot yet
        </p>
        <p className="mt-1 text-xs text-slate-400">
          {carAge === null ? "Car's last write unknown" : `Car last wrote ${formatAgo(carAge)} ago`}
          {snapAge !== null && ` · newest snapshot ${formatAgo(snapAge)} ago`}
          {nextIn !== null && (nextIn > 0 ? ` · next scheduled in ${formatAgo(nextIn)}` : " · scheduled snapshot due now")}
        </p>
        {note && <p role="status" className="mt-1 text-xs text-amber-300">{note}</p>}
      </div>
      <button
        className={buttonClass}
        disabled={!fresh || fresh.refreshing || waitFor !== null}
        onClick={() => void snapshotNow()}
        title="Takes a snapshot now, the same way the scheduled ones are taken, so the clips recorded since the last one show up."
      >
        {fresh?.refreshing ? (
          <><ProgressActivityIcon className="h-4 w-4 animate-spin" /> Taking snapshot…</>
        ) : waitFor !== null ? (
          `Snapshot now (in ${formatAgo(waitFor)})`
        ) : (
          "Snapshot now"
        )}
      </button>
    </section>
  )
}

/** How often the Snapshots page reloads itself while visible. */
export const AUTO_REFRESH_MS = 60_000

/** Word typed to confirm deleting snapshots that aren't fully verified. */
export const UNVERIFIED_DELETE_WORD = "DELETE"

/** Selected snapshots whose footage isn't fully verified on the archive
 *  server. Without verification data (verification off, no archive) there's
 *  nothing to warn about, so they don't count. */
function unverifiedCount(entries: SnapshotEntry[]): number {
  return entries.filter((e) => e.archive && e.archive.state !== "verified" && e.archive.state !== "nothing").length
}

export function ArchiveTag({ archive }: { archive?: ArchiveCoverage | null }) {
  if (!archive || archive.state === "nothing") return null
  const checked = archive.last_checked_unix ? ` Last checked ${formatDate(archive.last_checked_unix)}.` : ""
  const notArchived = archive.not_archived
    ? ` ${archive.not_archived} clip(s) are in a folder this device doesn't archive, so this snapshot is never deleted automatically.`
    : ""
  switch (archive.state) {
    case "verified":
      return (
        <span className={`${tagClass} border-emerald-400/30 bg-emerald-400/10 text-emerald-300`}
          title={`All ${archive.clips} clips were sent to the archive server and their SHA-256 checksums match the Pi's copy.${archive.converted ? ` ${archive.converted} of them were re-encoded by Sentry Manager (H.265/AV1); their originals were confirmed by checksum before or during conversion.` : ""}${archive.quick ? ` ${archive.quick} were checked with the quick check (size, frame index and spot checks of the video) instead of reading them back whole.` : ""}${archive.verified_twice === archive.clips ? " Every clip has been checked twice." : ""}${checked}`}>
          <CheckCircleIcon className="h-3 w-3" />
          Verified on archive{archive.verified_twice === archive.clips ? " ×2" : ""}
        </span>
      )
    case "pending":
      return (
        <span className={`${tagClass} border-sky-400/30 bg-sky-400/10 text-sky-300`}
          title={`Sent to the archive server; ${archive.pending} clip(s) waiting for their checksum check.${checked}`}>
          <ScheduleIcon className="h-3 w-3" />
          Sent · verifying
        </span>
      )
    case "failed":
      return (
        <span className={`${tagClass} border-red-400/30 bg-red-400/10 text-red-300`}
          title={`${archive.failed} clip(s) on the archive server didn't match the Pi's copy or were missing. They're sent again on the next archive.${checked}`}>
          <ErrorIcon className="h-3 w-3" />
          Checksum mismatch · re-sending
        </span>
      )
    case "partial":
      return (
        <span className={`${tagClass} border-amber-400/30 bg-amber-400/10 text-amber-300`}
          title={`${archive.verified} of ${archive.clips} clips verified on the archive server.${archive.unconfirmed ? ` ${archive.unconfirmed} clip(s) were re-encoded by Sentry Manager before their original could be checked, so they're on the server but unconfirmed; this snapshot is never deleted automatically.` : ""}${notArchived}${checked}`}>
          {archive.verified} / {archive.clips} verified
          {archive.pending > 0 && ` · ${archive.pending} queued`}
          {(archive.unconfirmed ?? 0) > 0 && ` · ${archive.unconfirmed} unconfirmed`}
        </span>
      )
    default:
      return (
        <span className={`${tagClass} border-white/10 bg-white/5 text-slate-400`}
          title={`None of this snapshot's ${archive.clips} clips are verified on the archive server yet.${notArchived}`}>
          {archive.pending > 0 ? `0 / ${archive.clips} verified · ${archive.pending} queued` : "Not archived yet"}
        </span>
      )
  }
}

export default function Snapshots() {
  const [snapshots, setSnapshots] = useState<SnapshotEntry[]>([])
  const [allocated, setAllocated] = useState(0)
  const [free, setFree] = useState<FreeSpace | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)
  const [sort, setSort] = useState("oldest")
  const [hoverPreview, setHoverPreview] = useState<{ id: string; through: boolean } | null>(null)
  const [focusPreview, setFocusPreview] = useState<{ id: string; through: boolean } | null>(null)
  const [deletion, setDeletion] = useState<Deletion | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [confirmWord, setConfirmWord] = useState("")
  const [completed, setCompleted] = useState(0)
  const inFlight = useRef(false)
  const requestId = useRef(0)
  const abort = useRef<AbortController | null>(null)
  const refresh = useCallback(async (force = false) => {
    if (inFlight.current && !force) return
    abort.current?.abort()
    const id = ++requestId.current
    inFlight.current = true
    const controller = new AbortController()
    abort.current = controller
    let timedOut = false
    const timeout = setTimeout(() => { timedOut = true; controller.abort() }, 20000)
    return Promise.all([
      fetch("/api/snapshots", { signal: controller.signal }),
      fetch("/api/backingfiles/free-space", { signal: controller.signal }),
    ]).then(async ([listResponse, spaceResponse]) => {
      if (!listResponse.ok) throw new Error(await responseError(listResponse, "Could not load snapshots"))
      const data = await listResponse.json()
      if (controller.signal.aborted || id !== requestId.current) return
      setSnapshots(data.snapshots ?? [])
      setAllocated(data.total_allocated_bytes ?? 0)
      setPending(data.sizes_pending === true)
      if (spaceResponse.ok) {
        const space = await spaceResponse.json()
        if (!controller.signal.aborted && id === requestId.current) setFree(space)
      }
    }).catch(e => {
      if (id === requestId.current && (!controller.signal.aborted || timedOut))
        setError(timedOut ? "Loading snapshots timed out. Please retry." : e instanceof Error ? e.message : "Could not load snapshots")
    }).finally(() => {
      clearTimeout(timeout)
      if (id === requestId.current) {
        inFlight.current = false
        if (!controller.signal.aborted || timedOut) setLoading(false)
      }
    })
  }, [])
  useEffect(() => {
    void refresh(true)
    return () => abort.current?.abort()
  }, [refresh])
  useEffect(() => {
    if (!pending || deleting) return
    const timer = setInterval(() => {
      if (!document.hidden) void refresh()
    }, 4000)
    return () => clearInterval(timer)
  }, [pending, deleting, refresh])
  // Keep the list current: new snapshots (every 58 minutes, at boot, before
  // each archive) and verification progress appear without a manual Refresh.
  // Every minute while the page is visible, and at once when it becomes
  // visible again. Never during a deletion or while its dialog is open; a
  // request already in flight is left alone (refresh() skips while one runs).
  const deletionOpen = deletion !== null
  const onSnapshotTaken = useCallback(() => { void refresh(true) }, [refresh])
  useEffect(() => {
    if (deleting || deletionOpen) return
    const timer = setInterval(() => {
      if (!document.hidden) void refresh()
    }, AUTO_REFRESH_MS)
    const onVisible = () => {
      if (!document.hidden) void refresh()
    }
    document.addEventListener("visibilitychange", onVisible)
    return () => {
      clearInterval(timer)
      document.removeEventListener("visibilitychange", onVisible)
    }
  }, [deleting, deletionOpen, refresh])
  const oldest = useMemo(
    () =>
      // Equal timestamps retain API order: its cumulative estimates use that prefix.
      [...snapshots].sort((a, b) => a.created_unix - b.created_unix),
    [snapshots],
  )
  const visible = sort === "oldest" ? oldest : [...oldest].reverse()

  function selectionFor(snapshot: SnapshotEntry, through: boolean): Deletion {
    const index = oldest.findIndex((s) => s.id === snapshot.id)
    const entries = through ? oldest.slice(0, index + 1) : [snapshot]
    return {
      entries,
      estimate: through || index === 0 ? snapshot.cumulative_reclaim_bytes : null,
      through,
    }
  }
  const previewTarget = hoverPreview ?? focusPreview
  const previewSnapshot = previewTarget && snapshots.find(s => s.id === previewTarget.id)
  const preview = deletion ?? (previewSnapshot && previewTarget ? selectionFor(previewSnapshot, previewTarget.through) : null)
  const previewIds = new Set(preview?.entries.map(s => s.id))
  function askDelete(snapshot: SnapshotEntry, through: boolean) {
    setError(null)
    setCompleted(0)
    setHoverPreview(null)
    setFocusPreview(null)
    setConfirmWord("")
    setDeletion(selectionFor(snapshot, through))
  }
  // Deleting footage that may exist nowhere else needs the word typed.
  const unverifiedSelected = deletion ? unverifiedCount(deletion.entries) : 0
  const confirmBlocked = unverifiedSelected > 0 && confirmWord.trim().toUpperCase() !== UNVERIFIED_DELETE_WORD
  async function confirmDelete() {
    if (!deletion || deleting || confirmBlocked) return
    abort.current?.abort()
    requestId.current++
    inFlight.current = false
    setDeleting(true)
    setError(null)
    let removed = 0
    try {
      // Use exactly the reviewed IDs; new snapshots cannot enter this selection.
      for (const entry of deletion.entries) {
        const controller = new AbortController()
        let timeout: ReturnType<typeof setTimeout> | undefined
        const deadline = new Promise<never>((_, reject) => {
          timeout = setTimeout(() => {
            reject(new Error(`Deletion of ${entry.id} timed out and may still complete. Check the refreshed list before selecting again.`))
            controller.abort()
          }, 30000)
        })
        try {
          await Promise.race([deadline, (async () => {
            const response = await fetch(`/api/snapshots/${encodeURIComponent(entry.id)}`, {
              method: "DELETE", signal: controller.signal,
            })
            if (!response.ok && response.status !== 404)
              throw new Error(await responseError(response, `Could not delete ${entry.id}`))
          })()])
        } finally {
          clearTimeout(timeout)
        }
        removed++
        setCompleted(removed)
        setSnapshots((old) => old.filter((s) => s.id !== entry.id))
      }
      setDeletion(null)
    } catch (e) {
      setError(
        `${removed} of ${deletion.entries.length} snapshots confirmed removed. ${e instanceof Error ? e.message : "Deletion failed"}`,
      )
      setDeletion(null)
    } finally {
      setDeleting(false)
      await refresh(true)
    }
  }
  const health = free?.storage_health
  const usage = free?.total_bytes ? Math.min(100, (free.used_bytes / free.total_bytes) * 100) : 0
  const color =
    health?.state === "fail"
      ? "bg-red-400"
      : health?.state === "warn"
        ? "bg-amber-400"
        : "bg-emerald-400"
  return (
    <div className="mx-auto max-w-5xl space-y-5">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-2xl font-bold text-slate-100">Snapshots</h1>
        <button
          className={buttonClass}
          disabled={loading || deleting}
          onClick={() => {
            setError(null)
            void refresh()
          }}
        >
          Refresh
        </button>
      </header>
      <section className="glass-card space-y-3 p-5">
        <div className="flex flex-wrap justify-between gap-3">
          <div>
            <h2 className="flex items-center gap-2 font-medium text-slate-200">
              <HardDriveIcon className="h-4 w-4" />
              {health?.message ?? "Storage managed automatically"}
            </h2>
            <p className="mt-1 text-sm text-slate-400">
              {snapshots.length} snapshots · {formatBytes(allocated)} allocated
            </p>
          </div>
          {free?.mounted && (
            <p className="text-sm text-slate-300">
              {formatBytes(free.available_bytes)} free of {formatBytes(free.total_bytes)}
            </p>
          )}
        </div>
        {free?.mounted && (
          <div
            className="h-2 overflow-hidden rounded-full bg-white/10"
            role="meter"
            aria-label="Storage used"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(usage)}
          >
            <div className={`h-full ${color}`} style={{ width: `${usage}%` }} />
          </div>
        )}
        {!!oldest.length && (
          <p className="text-sm text-slate-400">
            Snapshot dates: {formatDate(oldest[0].created_unix)} –{" "}
            {formatDate(oldest[oldest.length - 1].created_unix)}
          </p>
        )}
        <details className="text-sm text-slate-400">
          <summary className="cursor-pointer text-slate-300">How shared storage works</summary>
          <p className="mt-2">
            Old snapshots are removed automatically when space is needed. Snapshots share blocks, so
            removing one may free very little. The estimate below applies to deleting that snapshot
            and all older snapshots. Manual deletion is optional.
          </p>
        </details>
      </section>
      {error && (
        <p
          role="alert"
          className="rounded-xl border border-red-400/20 bg-red-400/10 p-3 text-sm text-red-300"
        >
          {error}
        </p>
      )}
      <div className={`sticky top-0 z-20 flex flex-wrap items-center justify-between gap-3 rounded-2xl border px-3 py-2 backdrop-blur ${preview ? "border-red-400/30 bg-[#20151b]" : "border-transparent bg-[#0a1117]/95"}`}>
        <p id="snapshot-delete-preview" role="status" aria-live="polite" className={`text-sm ${preview ? "text-red-200" : "text-slate-400"}`}>
          {preview ? `${preview.entries.length} ${preview.entries.length === 1 ? "snapshot" : "snapshots"} would be deleted${preview.estimate === null ? " · space estimate unavailable" : ` · ~${formatBytes(preview.estimate)} freed`}` : "Estimated space freed by deleting this and older"}
        </p>
        <SelectMenu label="Sort snapshots" value={sort} onChange={value => { setHoverPreview(null); setFocusPreview(null); setSort(value) }} align="end" options={[{ value: "oldest", label: "Oldest first" }, { value: "newest", label: "Newest first" }]} />
      </div>
      <LiveRow onSnapshotTaken={onSnapshotTaken} />
      {loading ? (
        <p role="status" className="p-8 text-slate-400">
          Loading snapshots…
        </p>
      ) : !visible.length ? (
        <p className="glass-card p-8 text-center text-slate-400">No snapshots on this device.</p>
      ) : (
        <ul className="glass-card divide-y divide-white/10 overflow-hidden">
          {visible.map((s) => (
            <li key={s.id} data-snapshot-id={s.id} data-delete-preview={previewIds.has(s.id) || undefined}
              className={`flex flex-wrap items-center justify-between gap-3 border-l-2 px-4 py-3 transition-colors ${previewIds.has(s.id) ? "border-l-red-400 bg-red-500/10 shadow-[inset_0_0_20px_rgba(239,68,68,0.04)]" : "border-l-transparent"}`}>
              <div className="min-w-48 flex-1">
                <p className="flex flex-wrap items-center gap-2 text-sm font-medium text-slate-200">
                  {formatDate(s.created_unix)}
                  <ArchiveTag archive={s.archive} />
                  {s.archive?.ready_to_delete && (
                    <span className={`${tagClass} border-white/10 bg-white/5 text-slate-400`}
                      title="Auto-delete is on and every clip is verified twice: this snapshot is deleted at the next archive cycle.">
                      Deleting after next archive
                    </span>
                  )}
                </p>
                <p className="mt-1 text-xs text-slate-400">
                  {s.id} ·{" "}
                  {s.cumulative_reclaim_bytes === null
                    ? pending
                      ? "Measuring space…"
                      : "Estimate unavailable"
                    : `~${formatBytes(s.cumulative_reclaim_bytes)} for this + ${s.older_count} older`}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                <button
                  className={buttonClass}
                  disabled={deleting}
                  aria-label={`Delete only ${s.id}`}
                  onMouseEnter={() => setHoverPreview({ id: s.id, through: false })}
                  onMouseLeave={() => setHoverPreview(null)}
                  onFocus={() => setFocusPreview({ id: s.id, through: false })}
                  onBlur={() => setFocusPreview(null)}
                  aria-describedby="snapshot-delete-preview"
                  onClick={() => askDelete(s, false)}
                >
                  <DeleteIcon className="h-4 w-4" />
                  Delete only this
                </button>
                {s.older_count > 0 && (
                  <button
                    className={buttonClass}
                    disabled={deleting}
                    aria-label={`Delete through ${s.id}`}
                    aria-describedby="snapshot-delete-preview"
                    onMouseEnter={() => setHoverPreview({ id: s.id, through: true })}
                    onMouseLeave={() => setHoverPreview(null)}
                    onFocus={() => setFocusPreview({ id: s.id, through: true })}
                    onBlur={() => setFocusPreview(null)}
                    onClick={() => askDelete(s, true)}
                  >
                    Delete through this date
                  </button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
      {deletion && (
        <Modal
          title={deletion.through ? "Delete through this date?" : "Delete this snapshot?"}
          onClose={() => setDeletion(null)}
          dismissable={!deleting}
          footer={
            <div className="flex justify-end gap-2">
              <button className={buttonClass} disabled={deleting} onClick={() => setDeletion(null)}>
                Cancel
              </button>
              <button
                className="rounded-xl bg-red-600 px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
                disabled={deleting || confirmBlocked}
                onClick={() => void confirmDelete()}
              >
                {deleting
                  ? `${completed} / ${deletion.entries.length} removed`
                  : "Delete permanently"}
              </button>
            </div>
          }
        >
          <p className="text-sm text-slate-200">
            {deletion.entries.length} {deletion.entries.length === 1 ? "snapshot" : "snapshots"}{" "}
            selected.
          </p>
          <p className="mt-2 text-sm text-slate-400">
            {formatDate(deletion.entries[0].created_unix)}
            {deletion.entries.length > 1
              ? ` – ${formatDate(deletion.entries[deletion.entries.length - 1].created_unix)}`
              : ""}
          </p>
          <p className="mt-3 text-sm text-slate-300">
            {deletion.estimate === null
              ? "Reclaimed space depends on blocks shared with remaining snapshots."
              : `Estimated space freed: ${formatBytes(deletion.estimate)}.`}
          </p>
          {unverifiedSelected > 0 && (
            <div className="mt-3 space-y-2 rounded-xl border border-amber-400/30 bg-amber-400/10 p-3">
              <p className="text-sm text-amber-300">
                {unverifiedSelected} of the selected {deletion.entries.length === 1 ? "snapshot is" : "snapshots are"} not
                fully verified on the archive server. Their footage may not be anywhere else.
              </p>
              <label className="block text-xs text-amber-200">
                Type {UNVERIFIED_DELETE_WORD} to delete them anyway
                <input
                  aria-label="Confirm deleting unverified snapshots"
                  className="mt-1 block w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2 text-sm text-slate-100"
                  value={confirmWord}
                  disabled={deleting}
                  autoComplete="off"
                  onChange={(event) => setConfirmWord(event.target.value)}
                />
              </label>
            </div>
          )}
          <p className="mt-3 text-sm text-red-300">
            Footage stored only in these snapshots will be lost. This cannot be undone. Live dashcam
            files are unaffected.
          </p>
          {deleting && <ProgressActivityIcon className="mt-3 h-5 w-5 animate-spin" />}
        </Modal>
      )}
    </div>
  )
}
