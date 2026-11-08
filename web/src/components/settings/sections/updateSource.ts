export const OFFICIAL_REPO = "Sentry-Six/Sentry-USB-Rusty"
export const DEFAULT_TEST_REPO = "skittles-fivem/Sentry-USB-Rusty"

export type UpdateSource = { repo: string; official: boolean; official_repo: string }

/** Mirrors the server's check: GitHub owner, one slash, repository name. */
export function isValidRepo(repo: string): boolean {
  return /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?\/(?!\.{1,2}$)[A-Za-z0-9._-]{1,100}$/.test(repo.trim())
}

/** Fork builds are tagged vX.Y.Z-test.N (see BUILD.md). */
export function isTestBuild(version: string | null): boolean {
  return !!version && /-test\.\d+$/.test(version.trim())
}

/** The custom repository an offered update comes from, or null for official. */
export function customRepo(repo: string | null | undefined): string | null {
  return repo && repo.toLowerCase() !== OFFICIAL_REPO.toLowerCase() ? repo : null
}
