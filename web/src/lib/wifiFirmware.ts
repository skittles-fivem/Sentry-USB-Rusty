/** Hide (`true`) or bring back (`false`) the Wi-Fi firmware update offer. */
export async function setWifiFirmwareOfferHidden(hidden: boolean): Promise<void> {
  const r = await fetch("/api/system/wifi-firmware/offer", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ hidden }),
  })
  if (!r.ok) {
    const body = await r.json().catch(() => ({}))
    throw new Error(body.error || "Could not save the setting.")
  }
}
