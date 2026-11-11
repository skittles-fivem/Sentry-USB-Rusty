import assert from 'node:assert/strict'
import test from 'node:test'
import { act, createElement } from 'react'
import { Window } from 'happy-dom'
import { ManagerLinkSection, WEB_PASSWORD_LINK, type ManagerLinkStatus } from './ManagerLinkSection.tsx'

const base: ManagerLinkStatus = {
  enabled: false, address: 'nas.local:10000', effective_address: 'nas.local:10000',
  web_password_set: true, paired: false, pairing: false, pairing_seconds_left: 0,
  fingerprint: null, fingerprint_display: null, state: 'off', message: 'Off.', connected: false,
  last_error: null, last_connected: null,
}

async function render(initial: ManagerLinkStatus, onWrite?: (method: string, url: string, body: unknown) => ManagerLinkStatus, log?: unknown) {
  const win = new Window({ url: 'http://localhost/' })
  const descriptors = ['window', 'document', 'navigator', 'IS_REACT_ACT_ENVIRONMENT'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const)
  for (const [key, value] of Object.entries({ window: win, document: win.document, navigator: win.navigator, IS_REACT_ACT_ENVIRONMENT: true })) {
    Object.defineProperty(globalThis, key, { configurable: true, value })
  }
  const oldFetch = globalThis.fetch
  let current = initial
  const writes: { method: string; url: string; body: unknown }[] = []
  globalThis.fetch = async (url, options) => {
    if (options?.method && options.method !== 'GET') {
      const body = options.body ? JSON.parse(String(options.body)) : undefined
      writes.push({ method: options.method, url: String(url), body })
      if (onWrite) current = onWrite(options.method, String(url), body)
    }
    if (String(url).endsWith('/log')) return Response.json(log ?? { entries: [], warnings: [] })
    return Response.json(current)
  }
  const { createRoot } = await import('react-dom/client')
  const container = win.document.createElement('div')
  win.document.body.append(container)
  const root = createRoot(container)
  await act(async () => root.render(createElement(ManagerLinkSection)))
  await act(async () => { await new Promise(r => setTimeout(r, 0)) })
  return {
    container, writes,
    async cleanup() {
      await act(async () => root.unmount())
      globalThis.fetch = oldFetch
      for (const [key, descriptor] of descriptors) {
        if (descriptor) Object.defineProperty(globalThis, key, descriptor)
        else Reflect.deleteProperty(globalThis, key)
      }
      win.close()
    },
  }
}

test('one switch turns the link on with the automatic address', async () => {
  const r = await render(base, () => ({ ...base, enabled: true, pairing: true, state: 'pairing', message: 'Waiting for Sentry Manager (15 min left).' }))
  try {
    const toggle = r.container.querySelector('input[type="checkbox"]') as unknown as HTMLInputElement
    assert.equal(toggle.checked, false)
    await act(async () => toggle.click())
    assert.deepEqual(r.writes[0], { method: 'PUT', url: '/api/manager-link', body: { enabled: true, address: 'nas.local:10000' } })
    assert.ok(r.container.textContent.includes('Waiting for Sentry Manager'))
    assert.ok(r.container.textContent.includes('Waiting to pair'))
    assert.ok(r.container.textContent.includes('with port 10000'))
    assert.ok(!r.container.textContent.includes('Automatic'), 'nothing is filled in automatically')
  } finally { await r.cleanup() }
})

test('without a web password it says so and links to where to set it', async () => {
  const r = await render({ ...base, enabled: true, web_password_set: false, state: 'needs_password', message: 'Set a web password first.' })
  try {
    const link = r.container.querySelector(`a[href="${WEB_PASSWORD_LINK}"]`)
    assert.ok(link, 'link to the web password step')
    assert.ok(r.container.textContent.includes('Set a web password first.'))
    assert.ok(!r.container.textContent.includes('Pair'))
  } finally { await r.cleanup() }
})

test('a paired link shows the grouped fingerprint and unpair needs a confirm', async () => {
  const display = '61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27'
  const paired = { ...base, enabled: true, paired: true, connected: true, state: 'connected' as const, message: 'Connected to Sentry Manager.', fingerprint: 'x', fingerprint_display: display }
  const r = await render(paired, () => ({ ...paired, paired: false, fingerprint: null, fingerprint_display: null, connected: false, state: 'pairing_expired', message: 'Not paired.' }))
  try {
    assert.ok(r.container.textContent.includes(display))
    assert.ok(r.container.textContent.includes('Connected'))
    const unpair = [...r.container.querySelectorAll('button')].find(b => b.textContent === 'Unpair')!
    await act(async () => unpair.click())
    assert.equal(r.writes.length, 0, 'first press only arms the confirm')
    const confirm = [...r.container.querySelectorAll('button')].find(b => b.textContent === 'Confirm unpair')!
    await act(async () => confirm.click())
    assert.deepEqual(r.writes[0], { method: 'POST', url: '/api/manager-link/unpair', body: undefined })
    assert.ok(!r.container.textContent.includes(display))
  } finally { await r.cleanup() }
})

test('a fingerprint mismatch is shown prominently with the fingerprint seen', async () => {
  const seen = '0123456789abcdef'.repeat(4)
  const warn = { at: 1_800_000_000, level: 'warn', kind: 'fingerprint_mismatch', msg: 'Possible fake Sentry Manager', address: '192.168.1.9:10000', seen }
  const log = {
    entries: [warn, { at: 1_799_999_000, level: 'info', kind: 'request', msg: 'Request served', method: 'GET', path: '/api/status', status: 200 }],
    warnings: [warn],
  }
  const r = await render({ ...base, enabled: true, paired: true, state: 'cert_changed', message: "Sentry Manager's certificate changed." }, undefined, log)
  try {
    const alert = r.container.querySelector('[role="alert"]')!
    assert.ok(alert.textContent.includes('Possible fake Sentry Manager'))
    assert.ok(alert.textContent.includes('192.168.1.9:10000'))
    assert.ok(alert.textContent.includes('0123 4567 89AB CDEF'))
    const toggle = [...r.container.querySelectorAll('button')].find(b => b.textContent === 'Show security log')!
    await act(async () => toggle.click())
    assert.ok(r.container.textContent.includes('GET /api/status → 200'))
  } finally { await r.cleanup() }
})

test('before the first pairing it shows the fingerprint to check and pairs on one tap', async () => {
  const fp = '61ecb6872005d76f9c85b9abee8c635c413381103bf5df251ec263fc331d6c27'
  const display = '61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27'
  const check = { fingerprint: fp, display, address: 'nas.local:10000', first: '61EC', middle: display.slice(5, -5), last: '6C27' }
  const confirm = { ...base, enabled: true, pairing: true, state: 'confirm_fingerprint' as const, message: "Check this matches the fingerprint on Sentry Manager's Sentry USB page.", check }
  const r = await render(confirm, () => ({ ...confirm, check: null, state: 'pairing', fingerprint: fp, fingerprint_display: display, message: 'Waiting for Sentry Manager (15 min left).' }))
  try {
    assert.ok(r.container.textContent.includes("Check this matches the fingerprint on Sentry Manager's Sentry USB page"))
    const big = [...r.container.querySelectorAll('span.text-2xl')].map(e => e.textContent)
    assert.deepEqual(big, ['61EC', '6C27'], 'first and last groups are large')
    assert.ok(r.container.textContent.includes('B687 2005'))
    assert.equal(r.writes.length, 0, 'nothing confirmed until the tap')
    const pair = [...r.container.querySelectorAll('button')].find(b => b.textContent === 'It matches, pair')!
    await act(async () => pair.click())
    assert.deepEqual(r.writes[0], { method: 'POST', url: '/api/manager-link/confirm', body: { fingerprint: fp } })
    assert.ok(r.container.textContent.includes('Waiting to pair'))
    assert.ok(!r.container.textContent.includes('It matches, pair'))
  } finally { await r.cleanup() }
})

test('a fingerprint that changed after confirming is flagged, and a failed read says so', async () => {
  const fp = 'ab'.repeat(32)
  const display = 'ABAB '.repeat(16).trim()
  const check = { fingerprint: fp, display, address: 'nas.local:10000', first: 'ABAB', middle: 'ABAB '.repeat(14).trim(), last: 'ABAB' }
  const r = await render({ ...base, enabled: true, pairing: true, state: 'confirm_fingerprint', fingerprint_changed: true, check,
    message: 'The fingerprint changed after you confirmed it, so nothing was sent.' })
  try {
    assert.ok(r.container.textContent.includes('changed after you confirmed it'))
    assert.ok(r.container.textContent.includes('It matches, pair'))
  } finally { await r.cleanup() }
  const f = await render({ ...base, enabled: true, pairing: true, state: 'check_failed', check_error: 'connection refused',
    message: "Can't read Sentry Manager's fingerprint at nas.local:10000 yet: connection refused." })
  try {
    assert.ok(f.container.textContent.includes("Can't reach Manager"))
    assert.ok(f.container.textContent.includes('Last error: connection refused'))
    assert.ok(!f.container.textContent.includes('It matches, pair'))
  } finally { await f.cleanup() }
})
