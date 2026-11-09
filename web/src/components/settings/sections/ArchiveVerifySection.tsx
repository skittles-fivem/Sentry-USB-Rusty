import { useEffect, useState } from "react"
import { CloudIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { InfoButton } from "@/components/ui/InfoButton"
import { SegPicker } from "@/components/ui/SegPicker"
import { Toggle } from "@/components/ui/Toggle"

type VerifyMode = "full" | "basic"

const MODE_OPTIONS: { value: VerifyMode; label: string }[] = [
  { value: "full", label: "Full (SHA-256)" },
  { value: "basic", label: "Basic (size and frames)" },
]

interface ArchiveVerifySettings {
  verify: boolean
  auto_delete: boolean
  trust_unconfirmed?: boolean
  full_readback?: boolean
  /** "basic": size and frames; "full": SHA-256. */
  mode?: VerifyMode
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

  async function save(change: Partial<Pick<ArchiveVerifySettings, "verify" | "auto_delete" | "trust_unconfirmed" | "full_readback" | "mode">>) {
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
  const mountedShare = settings?.archive_system === "cifs" || settings?.archive_system === "nfs"
  const mode: VerifyMode = settings?.mode === "basic" ? "basic" : "full"
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
      {mountedShare && (
        <div className="space-y-1.5">
          <div className="flex items-center gap-1.5">
            <p className="text-sm text-slate-200">Verification method</p>
            <InfoButton title="Full or basic verification">
              <p>
                <strong>Full</strong> compares SHA-256 checksums. When Sentry Manager's checksum record is on the share,
                the Pi uses it and reads nothing back. Clips without a record line yet wait for Sentry Manager, up to
                24 hours, and are then checked by the quick check, or read back whole if that option is on.
              </p>
              <p>
                <strong>Basic</strong> compares each clip's size and its frame index on the Pi and on the server: the video
                codec, the number of frames, the duration and the size of every frame. It reads the index only, usually
                well under 100 KB per clip, so a clip takes well under a second to check. It doesn't wait for Sentry
                Manager, and the Pi doesn't hash clips ahead of time. A clip Sentry Manager re-encoded passes when its
                frame count and duration match the original, or when it has one frame more or fewer and the same
                duration (Sentry Manager keeps the timing when the car's encoder skipped a frame).
              </p>
              <p>
                A cut-off, missing or rewritten copy can't pass the basic check. Damage that leaves every frame's
                size unchanged (bit rot inside the video) can, so a basic match is weaker than a checksum. Basic
                matches count toward auto-delete like full ones.
              </p>
            </InfoButton>
          </div>
          <SegPicker<VerifyMode>
            options={MODE_OPTIONS}
            value={mode}
            onChange={(next) => { if (next !== mode) void save({ mode: next }) }}
            disabled={!settings || saving || !settings.verify}
          />
          <p className="t-xs">
            {mode === "basic"
              ? "Fast: compares size and frames. Well under a second per clip."
              : "Thorough: SHA-256, using Sentry Manager's record when it's there."}
          </p>
        </div>
      )}
      {mountedShare && mode === "full" && (
        <Toggle
          checked={!!settings?.full_readback}
          disabled={!settings || saving || !settings.verify}
          onChange={(full_readback) => void save({ full_readback })}
          label="Read back whole clips (SMB/NFS)"
          sub="Slower, and downloads every clip again. Off uses the quick check."
          help={
            <InfoButton title="Quick check or full read-back">
              <p>
                An SMB/NFS share can't compute a checksum itself, so a full check means the Pi reads every clip back
                over the network. When Sentry Manager's checksum record is on the share, the Pi uses that instead and
                reads nothing back.
              </p>
              <p>
                Otherwise the quick check compares the clip's size, its frame index (the MP4 "moov" box, which holds
                every frame's size and timing, so frame count and frame rate must match), and four 64 KB samples of the
                video: about 300 KB per clip instead of the whole file. A cut-off or truncated copy can't pass, but
                damage confined to video between the samples can. Turn this on to read clips back whole instead.
              </p>
            </InfoButton>
          }
        />
      )}
      <Toggle
        checked={!!settings?.trust_unconfirmed}
        disabled={!settings || saving || noArchive || !settings.verify}
        onChange={(trust_unconfirmed) => void save({ trust_unconfirmed })}
        label="Trust Sentry Manager's earlier conversions"
        sub="Count clips it re-encoded before they could be checked as verified."
        help={
          <InfoButton title="Earlier conversions">
            <p>
              Sentry Manager now records each original clip's SHA-256 before it re-encodes it, so the Pi can verify
              converted clips by checksum. Clips it converted before that, without the Pi having checked them first,
              can't be verified this way: the original is gone. They show as "unconfirmed", and snapshots holding them
              are never deleted automatically.
            </p>
            <p>
              Turn this on to count those clips as verified, relying on Sentry Manager's own conversion checks for them.
              It only affects clips already marked unconfirmed; new conversions are still checked by checksum.
            </p>
          </InfoButton>
        }
      />
      {error && <p role="alert" className="text-xs text-rose-300">{error}</p>}
    </PrefCard>
  )
}
