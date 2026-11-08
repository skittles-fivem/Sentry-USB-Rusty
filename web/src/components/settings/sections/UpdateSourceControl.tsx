import { useEffect, useState } from "react"
import { InfoButton } from "@/components/ui/InfoButton"
import { DEFAULT_TEST_REPO, OFFICIAL_REPO, isValidRepo, type UpdateSource } from "./updateSource"

export function UpdateSourceControl({ onChange }: { onChange?: (source: UpdateSource) => void }) {
  const [source, setSource] = useState<UpdateSource | null>(null)
  const [custom, setCustom] = useState(false)
  const [repo, setRepo] = useState(DEFAULT_TEST_REPO)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  function apply(next: UpdateSource) {
    if (typeof next?.repo !== "string" || typeof next.official !== "boolean") throw new Error("Could not read the update source.")
    setSource(next)
    setCustom(!next.official)
    if (!next.official) setRepo(next.repo)
    onChange?.(next)
  }

  useEffect(() => {
    let active = true
    fetch("/api/system/update-source")
      .then(async response => {
        if (!response.ok) throw new Error()
        return response.json()
      })
      .then(data => { if (active) apply(data) })
      .catch(() => { if (active) setError("Could not load the update source. Reload to try again.") })
    return () => { active = false }
  // eslint-disable-next-line react-hooks/exhaustive-deps -- load once on mount
  }, [])

  async function save(nextRepo: string) {
    setSaving(true)
    setError(null)
    try {
      const response = await fetch("/api/system/update-source", {
        method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ repo: nextRepo }),
      })
      const data = await response.json().catch(() => ({}))
      if (!response.ok) throw new Error(data.error || "Could not save the update source. Try again.")
      apply(data)
    } catch (e) { setError(e instanceof Error ? e.message : "Could not save the update source. Try again.") }
    finally { setSaving(false) }
  }

  const trimmed = repo.trim()
  const valid = isValidRepo(trimmed)
  const unchanged = source !== null && !source.official && source.repo === trimmed
  const disabled = source === null || saving

  return <div className="flex flex-col gap-2">
    <div className="flex items-center gap-1">
      <span className="t-md">Update source</span>
      <InfoButton title="Update source">
        <p>Where updates are downloaded from. Official releases come from {source?.official_repo ?? OFFICIAL_REPO}.</p>
        <p>A custom repository is for testing a fork's builds. Tag them <code>v&lt;next patch&gt;-test.&lt;n&gt;</code> (for example v3.28.10-test.1 while official is v3.28.9), so the next official release always counts as newer. Switch back to Official to get that release in one update.</p>
        <p>Test builds are pre-releases, so they are never installed automatically. Install them with the button above.</p>
      </InfoButton>
    </div>
    <label className="flex items-center gap-2 text-xs text-slate-300">
      <input type="radio" name="update_source" checked={!custom} disabled={disabled} className="accent-blue-500"
        onChange={() => { setCustom(false); if (source && !source.official) void save("") }} />
      Official ({source?.official_repo ?? OFFICIAL_REPO})
    </label>
    <label className="flex items-center gap-2 text-xs text-slate-300">
      <input type="radio" name="update_source" checked={custom} disabled={disabled} className="accent-blue-500"
        onChange={() => setCustom(true)} />
      Custom repository
    </label>
    {custom && <div className="flex items-center gap-2 pl-5">
      <input type="text" aria-label="Custom update repository" value={repo} spellCheck={false} disabled={disabled}
        onChange={e => setRepo(e.target.value)} placeholder={DEFAULT_TEST_REPO}
        className={"min-w-0 flex-1 rounded-lg border bg-white/5 px-3 py-1.5 font-mono text-xs text-slate-100 outline-none transition focus:ring-1 disabled:opacity-50 " +
          (valid ? "border-white/10 focus:border-blue-500/50 focus:ring-blue-500/25" : "border-red-500/50 focus:border-red-500/50 focus:ring-red-500/25")} />
      <button type="button" disabled={disabled || !valid || unchanged} onClick={() => void save(trimmed)}
        className="shrink-0 rounded-lg bg-blue-500/15 px-3 py-1.5 text-[11px] font-medium text-blue-300 hover:bg-blue-500/25 disabled:opacity-50">
        Save
      </button>
    </div>}
    {source && !source.official && <p role="status" className="text-xs text-amber-300">
      Updates from {source.repo} (test builds)
    </p>}
    {custom && !valid && <p className="text-xs text-red-400">Enter owner/name, for example {DEFAULT_TEST_REPO}.</p>}
    {saving && <p role="status" className="t-xs">Saving…</p>}
    {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
  </div>
}
