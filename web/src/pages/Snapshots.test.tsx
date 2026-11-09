import assert from "node:assert/strict"
import test from "node:test"
import { act, createElement } from "react"
import { Window } from "happy-dom"
import Snapshots from "./Snapshots.tsx"

// The Live row polls its own two endpoints. Answer those here, so each
// test's fake fetch only sees the list/free-space requests it counts.
// Implemented as an accessor on globalThis.fetch: tests keep assigning and
// restoring fetch as before; the getter wraps whatever was assigned last.
const LIVE_STUB = Symbol("liveStub")
type Fetch = typeof fetch
let assignedFetch: Fetch = globalThis.fetch
let liveAnswer = { car_last_write_ts: null as number | null, next_scheduled_ts: null as number | null, server_ts: 1000 }
let freshAnswer = { snapshot_ts: null as number | null, refreshing: false, next_refresh_ts: null as number | null, checked_ts: null as number | null, server_ts: 1000 }
let refreshPosts = 0
Object.defineProperty(globalThis, "fetch", {
  configurable: true,
  get() {
    const target = assignedFetch
    const wrapped = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input)
      if (path.includes("/api/snapshots/live")) return Response.json(liveAnswer)
      if (path.includes("/api/clips/refresh")) { refreshPosts++; return Response.json(freshAnswer) }
      if (path.includes("/api/clips/freshness")) return Response.json(freshAnswer)
      return target(input, init)
    }) as Fetch & { [LIVE_STUB]?: Fetch }
    wrapped[LIVE_STUB] = target
    return wrapped
  },
  set(fn: Fetch & { [LIVE_STUB]?: Fetch }) {
    assignedFetch = fn[LIVE_STUB] ?? fn
  },
})

test("a delayed pre-delete list cannot resurrect a removed snapshot", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = {
    window: win,
    document: win.document,
    navigator: win.navigator,
    IS_REACT_ACT_ENVIRONMENT: true,
  }
  const originals = Object.keys(values).map(
    (key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const,
  )
  for (const [key, value] of Object.entries(values))
    Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  const entry = (id: number) => ({
    id: `snap-${id}`,
    created_unix: id * 100,
    older_count: id - 1,
    cumulative_reclaim_bytes: id * 1024,
  })
  let listCalls = 0
  let stale: ((response: Response) => void) | undefined
  const deleted: string[] = []
  globalThis.fetch = async (input, init) => {
    const path = String(input)
    if (init?.method === "DELETE") {
      deleted.push(path)
      return Response.json({})
    }
    if (path.includes("free-space"))
      return Response.json({
        mounted: true,
        total_bytes: 1000,
        used_bytes: 950,
        available_bytes: 50,
      })
    listCalls++
    if (listCalls === 2)
      return new Promise((resolve) => {
        stale = resolve
      })
    return Response.json({
      snapshots: listCalls === 1 ? [entry(1), entry(2)] : [entry(2), entry(3)],
    })
  }
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div")
  win.document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(createElement(Snapshots)))
    await act(async () =>
      [...container.querySelectorAll("button")].find((b) => b.textContent === "Refresh")!.click(),
    )
    assert.ok(stale)
    await act(async () =>
      container.querySelector<HTMLElement>('[aria-label="Delete only snap-1"]')!.click(),
    )
    await act(async () =>
      [...win.document.querySelectorAll("button")]
        .find((b) => b.textContent === "Delete permanently")!
        .click(),
    )
    await act(async () => stale!(Response.json({ snapshots: [entry(1), entry(2)] })))
    assert.deepEqual(deleted, ["/api/snapshots/snap-1"])
    assert.equal(listCalls, 3)
    assert.equal(container.querySelector('[aria-label="Delete only snap-1"]'), null)
    assert.ok(container.querySelector('[aria-label="Delete only snap-3"]'))
  } finally {
    await act(async () => root.unmount())
    globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
    win.close()
  }
})

test("snapshots finish loading after StrictMode cancels the first mount request", async () => {
  const { StrictMode } = await import("react")
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  globalThis.fetch = async input => String(input).includes("free-space")
    ? Response.json({ mounted: true, total_bytes: 1e12, available_bytes: 5e10, used_bytes: 95e10 })
    : Response.json({ snapshots: [{ id: "snap-existing", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024 }] })
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(createElement(StrictMode, null, createElement(Snapshots))))
    assert.ok(container.querySelector('[aria-label="Delete only snap-existing"]'))
    assert.ok(!container.textContent.includes("Loading snapshots"))
  } finally {
    await act(async () => root.unmount()); globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("delete preview follows exact chronological IDs in both sort orders without deleting", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  const methods: string[] = []
  globalThis.fetch = async (input, init) => {
    methods.push(init?.method ?? "GET")
    return String(input).includes("free-space") ? Response.json({ mounted: false }) : Response.json({ snapshots: [
      { id: "newest", created_unix: 300, older_count: 2, cumulative_reclaim_bytes: 4096 },
      { id: "oldest", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024 },
      { id: "middle", created_unix: 200, older_count: 1, cumulative_reclaim_bytes: 2048 },
    ] })
  }
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  const highlighted = () => [...container.querySelectorAll('[data-delete-preview="true"]')].map(row => row.getAttribute('data-snapshot-id'))
  try {
    await act(async () => root.render(createElement(Snapshots)))
    const through = container.querySelector<HTMLElement>('[aria-label="Delete through middle"]')!
    await act(async () => through.dispatchEvent(new win.MouseEvent("mouseover", { bubbles: true })))
    assert.deepEqual(highlighted(), ["oldest", "middle"])
    assert.match(container.querySelector('#snapshot-delete-preview')!.textContent, /2 snapshots.*2.0 KB/)
    await act(async () => through.dispatchEvent(new win.MouseEvent("mouseout", { bubbles: true })))
    assert.deepEqual(highlighted(), [])
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Sort snapshots"]')!.click())
    await act(async () => win.document.querySelector<HTMLElement>('[data-value="newest"]')!.click())
    await act(async () => through.focus())
    assert.deepEqual(highlighted(), ["middle", "oldest"])
    await act(async () => through.click())
    assert.match(win.document.querySelector('[role="dialog"]')!.textContent, /2 snapshots selected/)
    assert.deepEqual(highlighted(), ["middle", "oldest"])
    assert.ok(methods.every(method => method === 'GET'))
  } finally {
    await act(async () => root.unmount()); globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("tied timestamps preserve the API reclaim prefix in both sorts and delete exactly those IDs", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  const deleted: string[] = []
  // Legacy servers may return ties in directory order, not lexical ID order.
  const entries = [
    { id: "snap-200", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024 },
    { id: "snap-100", created_unix: 100, older_count: 1, cumulative_reclaim_bytes: 3072 },
    { id: "snap-300", created_unix: 300, older_count: 2, cumulative_reclaim_bytes: 8192 },
  ]
  globalThis.fetch = async (input, init) => {
    const path = String(input)
    if (init?.method === "DELETE") { deleted.push(path); return Response.json({}) }
    return path.includes("free-space") ? Response.json({ mounted: false })
      : Response.json({ snapshots: entries.filter(entry => !deleted.includes(`/api/snapshots/${entry.id}`)) })
  }
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  const highlighted = () => [...container.querySelectorAll('[data-delete-preview="true"]')].map(row => row.getAttribute("data-snapshot-id"))
  try {
    await act(async () => root.render(createElement(Snapshots)))
    const through = container.querySelector<HTMLElement>('[aria-label="Delete through snap-100"]')!
    await act(async () => through.focus())
    assert.deepEqual(highlighted(), ["snap-200", "snap-100"])
    assert.match(container.querySelector("#snapshot-delete-preview")!.textContent, /2 snapshots.*3.0 KB/)
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Sort snapshots"]')!.click())
    await act(async () => win.document.querySelector<HTMLElement>('[data-value="newest"]')!.click())
    await act(async () => through.focus())
    assert.deepEqual(highlighted(), ["snap-100", "snap-200"])
    await act(async () => through.click())
    assert.match(win.document.querySelector('[role="dialog"]')!.textContent, /2 snapshots selected/)
    assert.deepEqual(deleted, [])
    await act(async () => [...win.document.querySelectorAll("button")].find(button => button.textContent === "Delete permanently")!.click())
    assert.deepEqual(deleted, ["/api/snapshots/snap-200", "/api/snapshots/snap-100"])
    assert.ok(container.querySelector('[data-snapshot-id="snap-300"]'))
  } finally {
    await act(async () => root.unmount()); globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("a timed out deletion unlocks the modal, preserves unknown IDs and never continues or retries the batch", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch, oldTimeout = globalThis.setTimeout, oldClearTimeout = globalThis.clearTimeout
  const deadlines = new Map<number, () => void>()
  let timerId = 10000
  globalThis.setTimeout = ((callback: () => void, delay?: number, ...args: unknown[]) => {
    if (delay !== 30000) return oldTimeout(callback, delay, ...args)
    const id = timerId++
    deadlines.set(id, () => { deadlines.delete(id); callback() })
    return id
  }) as typeof setTimeout
  globalThis.clearTimeout = ((id: ReturnType<typeof setTimeout>) => {
    if (typeof id === "number" && deadlines.has(id)) deadlines.delete(id)
    else oldClearTimeout(id)
  }) as typeof clearTimeout
  const deletes: string[] = []
  let stalled: ((response: Response) => void) | undefined
  let signal: AbortSignal | undefined
  let listCalls = 0
  globalThis.fetch = async (input, init) => {
    const path = String(input)
    if (init?.method === "DELETE") {
      deletes.push(path)
      if (deletes.length === 2) {
        signal = init.signal as AbortSignal
        // Deliberately ignore abort: a late server reply must not continue the batch.
        return new Promise<Response>(resolve => { stalled = resolve })
      }
      return Response.json({})
    }
    if (path.includes("free-space")) return Response.json({ mounted: false })
    listCalls++
    return Response.json({ snapshots: (listCalls === 1 ? [1, 2, 3] : [2, 3]).map(id => ({
      id: `snap-${id}`, created_unix: id * 100, older_count: id - 1, cumulative_reclaim_bytes: id * 1024,
    })) })
  }
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(createElement(Snapshots)))
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Delete through snap-3"]')!.click())
    await act(async () => [...win.document.querySelectorAll("button")].find(button => button.textContent === "Delete permanently")!.click())
    assert.deepEqual(deletes, ["/api/snapshots/snap-1", "/api/snapshots/snap-2"])
    assert.match(win.document.querySelector('[role="dialog"]')!.textContent, /1 \/ 3 removed/)
    assert.equal(deadlines.size, 1)
    await act(async () => [...deadlines.values()][0]())
    assert.equal(signal?.aborted, true)
    assert.equal(win.document.querySelector('[role="dialog"]'), null)
    assert.match(container.querySelector('[role="alert"]')!.textContent, /1 of 3 snapshots confirmed removed.*snap-2 timed out and may still complete/)
    assert.equal(listCalls, 2)
    assert.equal(container.querySelector('[data-snapshot-id="snap-1"]'), null)
    assert.ok(container.querySelector('[data-snapshot-id="snap-2"]'))
    assert.ok(container.querySelector('[data-snapshot-id="snap-3"]'))
    await act(async () => stalled!(Response.json({})))
    assert.deepEqual(deletes, ["/api/snapshots/snap-1", "/api/snapshots/snap-2"])
    assert.equal(deadlines.size, 0)
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Delete through snap-3"]')!.click())
    assert.match(win.document.querySelector('[role="dialog"]')!.textContent, /2 snapshots selected/)
    assert.equal(deletes.length, 2, "retry requires a fresh confirmation")
  } finally {
    await act(async () => root.unmount())
    globalThis.fetch = oldFetch; globalThis.setTimeout = oldTimeout; globalThis.clearTimeout = oldClearTimeout
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("archive tags show verification state and the delete dialog warns about unverified footage", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  const coverage = (state: string, extra: object = {}) => ({
    state, clips: 4, verified: 0, verified_twice: 0, pending: 0, failed: 0, not_archived: 0,
    ready_to_delete: false, last_checked_unix: 50, ...extra,
  })
  globalThis.fetch = async input => String(input).includes("free-space") ? Response.json({ mounted: false }) : Response.json({ snapshots: [
    { id: "twice", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024,
      archive: coverage("verified", { verified: 4, verified_twice: 4, ready_to_delete: true }) },
    { id: "bad", created_unix: 200, older_count: 1, cumulative_reclaim_bytes: 2048, archive: coverage("failed", { failed: 1 }) },
    { id: "half", created_unix: 300, older_count: 2, cumulative_reclaim_bytes: 4096, archive: coverage("partial", { verified: 2 }) },
    { id: "off", created_unix: 400, older_count: 3, cumulative_reclaim_bytes: 8192 },
  ] })
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  const row = (id: string) => container.querySelector(`[data-snapshot-id="${id}"]`)!.textContent
  try {
    await act(async () => root.render(createElement(Snapshots)))
    assert.match(row("twice"), /Verified on archive ×2/)
    assert.match(row("twice"), /Deleting after next archive/)
    assert.match(row("bad"), /Checksum mismatch · re-sending/)
    assert.match(row("half"), /2 \/ 4 verified/)
    assert.doesNotMatch(row("off"), /archive/i, "no tag when verification is off")

    // Only the verified snapshot selected: no warning.
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Delete only twice"]')!.click())
    assert.doesNotMatch(win.document.body.textContent, /not\s+fully verified/)
    assert.equal(win.document.querySelector('[aria-label="Confirm deleting unverified snapshots"]'), null, "asked to type for a verified snapshot")
    assert.equal([...win.document.querySelectorAll("button")].find(b => b.textContent === "Delete permanently")!.disabled, false)
    await act(async () => [...win.document.querySelectorAll("button")].find(b => b.textContent === "Cancel")!.click())

    // Through "half": two of the three aren't fully verified.
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Delete through half"]')!.click())
    assert.match(win.document.body.textContent, /2 of the selected snapshots are not\s+fully verified/)
    // Deleting unverified snapshots needs DELETE typed first.
    const button = () => [...win.document.querySelectorAll("button")].find(b => b.textContent === "Delete permanently")!
    assert.equal(button().disabled, true, "delete enabled before typing the word")
    const input = win.document.querySelector<HTMLInputElement>('[aria-label="Confirm deleting unverified snapshots"]')!
    const type = async (value: string) => act(async () => {
      Object.getOwnPropertyDescriptor(win.HTMLInputElement.prototype, "value")!.set!.call(input, value)
      input.dispatchEvent(new win.Event("input", { bubbles: true }))
    })
    await type("delet")
    assert.equal(button().disabled, true, "a partial word enabled delete")
    await type("delete")
    assert.equal(button().disabled, false, "typing the word (any case) should enable delete")
  } finally {
    await act(async () => root.unmount()); globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("the list refreshes itself when the page becomes visible, but not with the delete dialog open", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  let lists = 0
  globalThis.fetch = async input => {
    if (String(input).includes("free-space")) return Response.json({ mounted: false })
    lists++
    const snaps = [{ id: "snap-1", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024 }]
    if (lists > 1) snaps.push({ id: "snap-2", created_unix: 200, older_count: 1, cumulative_reclaim_bytes: 2048 })
    return Response.json({ snapshots: snaps })
  }
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(createElement(Snapshots)))
    assert.equal(lists, 1)
    assert.equal(container.querySelector('[data-snapshot-id="snap-2"]'), null)
    // Coming back to the tab reloads the list; the new snapshot shows up.
    await act(async () => { win.document.dispatchEvent(new win.Event("visibilitychange")) })
    assert.equal(lists, 2)
    assert.ok(container.querySelector('[data-snapshot-id="snap-2"]'), "new snapshot not shown after refresh")
    // With the delete dialog open, the list is left alone.
    await act(async () => container.querySelector<HTMLElement>('[aria-label="Delete only snap-1"]')!.click())
    await act(async () => { win.document.dispatchEvent(new win.Event("visibilitychange")) })
    assert.equal(lists, 2, "refreshed under the open delete dialog")
  } finally {
    await act(async () => root.unmount()); globalThis.fetch = oldFetch
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})

test("the Live row shows whether the car is recording, the schedule, and takes a snapshot on demand", async () => {
  const win = new Window({ url: "http://localhost/snapshots" })
  const values = { window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true }
  const originals = Object.keys(values).map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, value })
  const oldFetch = globalThis.fetch
  globalThis.fetch = async input => String(input).includes("free-space")
    ? Response.json({ mounted: false })
    : Response.json({ snapshots: [{ id: "snap-1", created_unix: 100, older_count: 0, cumulative_reclaim_bytes: 1024 }] })
  liveAnswer = { car_last_write_ts: 1000 - 12, next_scheduled_ts: 1000 + 35 * 60, server_ts: 1000 }
  freshAnswer = { snapshot_ts: 1000 - 23 * 60, refreshing: false, next_refresh_ts: null, checked_ts: null, server_ts: 1000 }
  refreshPosts = 0
  const { createRoot } = await import("react-dom/client")
  const container = win.document.createElement("div"); win.document.body.append(container)
  const root = createRoot(container)
  const liveText = () => container.querySelector('[aria-label="Live"]')!.textContent
  const button = () => [...container.querySelectorAll<HTMLButtonElement>('[aria-label="Live"] button')][0]
  try {
    await act(async () => root.render(createElement(Snapshots)))
    await act(async () => { await new Promise(r => setTimeout(r, 0)) })
    assert.match(liveText(), /Live · recording/)
    assert.match(liveText(), /Car last wrote 12 s ago · newest snapshot 23 min ago · next scheduled in 35 min/)
    assert.equal(button().disabled, false)
    // Snapshot now: starts an on-demand snapshot and shows progress.
    freshAnswer = { ...freshAnswer, refreshing: true }
    await act(async () => button().click())
    assert.equal(refreshPosts, 1)
    assert.match(button().textContent, /Taking snapshot/)
    assert.equal(button().disabled, true)
    // Rate-limited: says when it's available again.
    freshAnswer = { snapshot_ts: 1000 - 60, refreshing: false, next_refresh_ts: 1000 + 120, checked_ts: 1000, server_ts: 1000 }
    liveAnswer = { car_last_write_ts: 1000 - 4000, next_scheduled_ts: null, server_ts: 1000 }
    // A parked car reads as not recording; the button says when it's next allowed.
    await act(async () => root.unmount())
    const container2 = win.document.createElement("div"); win.document.body.append(container2)
    const root2 = createRoot(container2)
    await act(async () => root2.render(createElement(Snapshots)))
    await act(async () => { await new Promise(r => setTimeout(r, 0)) })
    const text2 = container2.querySelector('[aria-label="Live"]')!.textContent
    assert.match(text2, /Live · not recording/)
    assert.match(text2, /Car last wrote 1 h 7 min ago/)
    assert.match(text2, /Snapshot now \(in 2 min\)/)
    await act(async () => root2.unmount())
  } finally {
    globalThis.fetch = oldFetch
    liveAnswer = { car_last_write_ts: null, next_scheduled_ts: null, server_ts: 1000 }
    freshAnswer = { snapshot_ts: null, refreshing: false, next_refresh_ts: null, checked_ts: null, server_ts: 1000 }
    for (const [key, descriptor] of originals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else Reflect.deleteProperty(globalThis, key) }
    win.close()
  }
})
