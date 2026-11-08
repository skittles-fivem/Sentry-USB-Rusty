import { useEffect, useState } from "react"
import { CloudIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { InfoButton } from "@/components/ui/InfoButton"
import { Toggle } from "@/components/ui/Toggle"

interface ArchiveVerifySettings {
  verify: boolean
  auto_delete: boolean
  archive_system: string
  recent_clips_archived: boolean
}

/** Settings for SHA-256 verification of archived clips and the optional
 *  auto-delete of snapshots verified twice. archiveloop reads both from
 *  sentryusb.conf at every archive cycle, so changes apply without a restart. */
export function ArchiveVerifySection() {
  const [settings, setSettings] = useState<ArchiveVerifySettings | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let active = true
    fetch("/api/archive-verify/settings")
      .then(async (response) => {
        if (!response.ok) throw new Error()
        return response.json()
      })
      .then((data) => { if (active) setSettings(data) })
      .catch(() => { if (active) setError("Could not load archive verification settings. Reload to try again.") })
    return () => { active = false }
  }, [])

  async function save(change: Partial<Pick<ArchiveVerifySettings, "verify" | "auto_delete">>) {
    setSaving(true)
    setError(null)
    try {
      const response = await fetch("/api/archive-verify/settings", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(change),
      })
      const data = await response.json().catch(() => null)
      if (!response.ok) throw new Error(data?.error ?? "save failed")
      setSettings(data)
    } catch {
      setError("Could not save archive verification settings. Try again.")
    } finally {
      setSaving(false)
    }
  }

  const noArchive = settings?.archive_system === "none"
  return (
    <PrefCard
      icon={<CloudIcon className="h-3.5 w-3.5" />}
      halo="accent"
      title="Archive verification"
      help={
        <InfoButton title="Archive verification">
          <p>
            After each archive, SentryUSB computes a SHA-256 checksum of every clip it sent, once from the
            copy on the Pi and once from the copy on the archive server, and compares them. Snapshots whose
            clips all match are tagged “Verified on archive” on the Snapshots page.
          </p>
          <p>
            A clip whose server copy doesn’t match or is missing is sent again on the next archive. A
            mismatched server copy is renamed with a “.mismatch” suffix, never deleted.
          </p>
          <p>
            Checking runs after the car has its USB drive back. On a mounted share (SMB/NFS) the Pi reads each
            clip back from the server; over rsync the server computes the checksums itself.
          </p>
        </InfoButton>
      }
    >
      {noArchive && <p className="t-xs">Set up an archive server to use verification.</p>}
      <Toggle
        checked={!!settings?.verify}
        disabled={!settings || saving || noArchive}
        onChange={(verify) => void save({ verify })}
        label="Verify archived clips (SHA-256)"
        sub="Compare each clip on the archive server with the Pi’s copy."
      />
      <Toggle
        checked={!!settings?.auto_delete}
        disabled={!settings || saving || noArchive || !settings.verify}
        onChange={(auto_delete) => void save({ auto_delete })}
        label="Delete snapshots once verified twice"
        sub="Frees space on the Pi after the archive server has every clip."
        help={
          <InfoButton title="Delete verified snapshots">
            <p>
              A snapshot is deleted from the Pi only when every clip in it has matched the archive server’s
              copy twice, with the second check at least an hour after the first.
            </p>
            <ul className="list-disc space-y-2 pl-5">
              <li>The newest snapshot is always kept.</li>
              <li>
                A snapshot holding any clip from a folder that isn’t archived is kept
                {settings && !settings.recent_clips_archived ? " (Recent Clips aren’t archived on this device, so snapshots with them are kept)" : ""}.
              </li>
              <li>
                Deleting waits for the snapshot lock, so it never runs while a snapshot is being taken, linked
                or cleaned up, and each snapshot is re-checked just before it’s deleted.
              </li>
            </ul>
            <p>This cannot be undone. The archive server then holds the only copy of that footage.</p>
          </InfoButton>
        }
      />
      {error && <p role="alert" className="text-xs text-rose-300">{error}</p>}
    </PrefCard>
  )
}
