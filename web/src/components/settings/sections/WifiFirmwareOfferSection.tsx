import { useCallback, useEffect, useState } from "react"
import { WifiIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { Toggle } from "@/components/ui/Toggle"
import type { WifiFirmwareStatus } from "@/components/dashboard/WifiFirmwareModal"
import { setWifiFirmwareOfferHidden } from "@/lib/wifiFirmware"

/**
 * Brings back (or hides) the dashboard's Wi-Fi firmware update offer after
 * "Don't ask again". Shown only on boards the update applies to that aren't
 * already on the newer firmware.
 */
export function WifiFirmwareOfferSection() {
  const [status, setStatus] = useState<WifiFirmwareStatus | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState("")

  useEffect(() => {
    fetch("/api/system/wifi-firmware")
      .then((r) => r.json())
      .then((d: WifiFirmwareStatus) => setStatus(d))
      .catch(() => {})
  }, [])

  const persist = useCallback((offer: boolean) => {
    setSaving(true)
    setError("")
    setWifiFirmwareOfferHidden(!offer)
      .then(() => setStatus((s) => (s ? { ...s, offer_hidden: !offer } : s)))
      .catch((e: Error) => setError(e.message))
      .finally(() => setSaving(false))
  }, [])

  if (!status || !status.supported_board || status.up_to_date) return null

  const offered = !status.offer_hidden
  return (
    <PrefCard icon={<WifiIcon className="h-3.5 w-3.5" />} halo="slate" title="Wi-Fi firmware">
      <Toggle
        label="Offer the Wi-Fi firmware update"
        checked={offered}
        disabled={saving}
        onChange={persist}
        sub={
          offered
            ? `The dashboard offers firmware ${status.target_version} for the built-in Wi-Fi.`
            : "Hidden. The built-in Wi-Fi stays on its current firmware."
        }
      />
      {error && (
        <p role="alert" className="text-xs text-rose-300">
          {error}
        </p>
      )}
    </PrefCard>
  )
}
