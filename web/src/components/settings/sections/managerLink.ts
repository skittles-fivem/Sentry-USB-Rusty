/** `61EC B687 …`: groups of four uppercase hex digits, the format both
 * Sentry USB and Sentry Manager show fingerprints in. */
export function groupFingerprint(fp: string): string {
  return (fp.toUpperCase().match(/.{1,4}/g) ?? []).join(" ")
}
