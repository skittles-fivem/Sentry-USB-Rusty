import assert from 'node:assert/strict'
import test from 'node:test'
import { act, createElement, StrictMode } from 'react'
import { Window } from 'happy-dom'
import { UpdateSourceControl } from './UpdateSourceControl.tsx'

const OFFICIAL = 'Sentry-Six/Sentry-USB-Rusty'
const FORK = 'skittles-fivem/Sentry-USB-Rusty'

test('switching to a test fork and back writes REPO and labels the source', async () => {
  const win = new Window({ url: 'http://localhost/' })
  const descriptors = ['window', 'document', 'navigator', 'IS_REACT_ACT_ENVIRONMENT'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries({ window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true })) {
    Object.defineProperty(globalThis, key, { configurable: true, value })
  }
  const oldFetch = globalThis.fetch
  const writes: unknown[] = []
  let repo = OFFICIAL
  const state = () => Response.json({ repo, official: repo === OFFICIAL, official_repo: OFFICIAL })
  globalThis.fetch = async (_url, options) => {
    if (options?.method === 'PUT') {
      const body = JSON.parse(String(options.body))
      writes.push(body)
      repo = body.repo || OFFICIAL
    }
    return state()
  }
  const changes: string[] = []
  const { createRoot } = await import('react-dom/client')
  const container = win.document.createElement('div')
  win.document.body.append(container)
  const root = createRoot(container)
  try {
    await act(async () => root.render(createElement(StrictMode, {}, createElement(UpdateSourceControl, { onChange: s => changes.push(s.repo) }))))
    const [official, custom] = [...container.querySelectorAll('input[type="radio"]')] as unknown as HTMLInputElement[]
    assert.equal(official.checked, true)
    assert.ok(!container.textContent.includes('test builds'))
    await act(async () => custom.click())
    const input = container.querySelector('input[type="text"]') as unknown as HTMLInputElement
    assert.equal(input.value, FORK, 'prefilled with the test fork')
    assert.equal(writes.length, 0, 'choosing Custom alone saves nothing')
    const save = [...container.querySelectorAll('button')].find(b => b.textContent === 'Save')!
    await act(async () => save.click())
    assert.deepEqual(writes[0], { repo: FORK })
    assert.ok(container.textContent.includes(`Updates from ${FORK} (test builds)`))
    await act(async () => official.click())
    assert.deepEqual(writes[1], { repo: '' })
    assert.ok(!container.textContent.includes('test builds'))
    assert.equal(changes.at(-1), OFFICIAL)
  } finally {
    await act(async () => root.unmount())
    globalThis.fetch = oldFetch
    for (const [key, descriptor] of descriptors) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else Reflect.deleteProperty(globalThis, key)
    }
    win.close()
  }
})
