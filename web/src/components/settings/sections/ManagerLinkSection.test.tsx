import assert from 'node:assert/strict'
import test from 'node:test'
import { act, createElement } from 'react'
import { Window } from 'happy-dom'
import { ManagerLinkSection, WEB_PASSWORD_LINK, type ManagerLinkStatus } from './ManagerLinkSection.tsx'

const base: ManagerLinkStatus = {
  enabled: false, address: '', effective_address: null,
  web_password_set: true, paired: false, pairing: false, pairing_seconds_left: 0,
  fingerprint: null, fingerprint_display: null, state: 'off', message: 'Off.', connected: false,
  last_error: null, last_connected: null,
}
const NOT_PAIRED = 'Not paired. In Sentry Manager, open Settings → Sentry USB → Add a Sentry USB and press Start pairing.'
const WRONG = 'Wrong pairing code or password. Check the web username and password you entered in Sentry Manager.'

function input(c: { querySelector(s: string): unknown }, id: string) {
  return c.querySelector(`#${id}`) as unknown as HTMLInputElement
}

/** Types into a React-controlled input. */
async function type(el: HTMLInputElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value')!.set!
    setter.call(el, value)
    el.dispatchEvent(new (el.ownerDocument.defaultView as unknown as typeof globalThis).Event('input', { bubbles: true }))
  })
}

function button(c: { querySelectorAll(s: string): Iterable<Element> }, text: string) {
  return [...c.querySelectorAll('button')].find(b => b.textContent === text) as HTMLButtonElement | undefined
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

test('the switch turns the link on; the address starts empty with a hint, and nothing is filled in', async () => {
  const r = await render(base, () => ({ ...base, enabled: true, state: 'needs_pairing', message: NOT_PAIRED }))
  try {
    const toggle = r.container.querySelector('input[type="checkbox"]') as unknown as HTMLInputElement
    assert.equal(toggle.checked, false)
    await act(async () => toggle.click())
    assert.deepEqual(r.writes[0], { method: 'PUT', url: '/api/manager-link', body: { enabled: true, address: '' } })
    const address = input(r.container, 'manager-address')
    assert.equal(address.value, '')
    assert.equal(address.placeholder, '192.168.1.5:10000')
    assert.ok(input(r.container, 'manager-code'), 'a pairing code field')
    assert.ok(button(r.container, 'Pair')?.disabled, 'Pair waits for an address and a code')
    assert.ok(r.container.textContent.includes(NOT_PAIRED))
    assert.ok(!r.container.textContent.includes('Automatic'), 'nothing is filled in automatically')
    assert.ok(!r.container.textContent.includes('It matches'), 'no fingerprint to confirm')
  } finally { await r.cleanup() }
})

test('Pair sends the address and the code as typed, then forgets the code', async () => {
  const notPaired = { ...base, enabled: true, state: 'needs_pairing' as const, message: NOT_PAIRED }
  const r = await render(notPaired, () => ({ ...notPaired, address: '192.168.1.5:10000', effective_address: '192.168.1.5:10000',
    pairing: true, pairing_seconds_left: 900, state: 'pairing', message: 'Pairing with Sentry Manager at 192.168.1.5:10000 (15 min left)…' }))
  try {
    await type(input(r.container, 'manager-address'), ' 192.168.1.5:10000 ')
    await type(input(r.container, 'manager-code'), 'k7qp m2xd-9wrt')
    const pair = button(r.container, 'Pair')!
    assert.equal(pair.disabled, false)
    await act(async () => pair.click())
    assert.deepEqual(r.writes[0], { method: 'POST', url: '/api/manager-link/pair', body: { address: '192.168.1.5:10000', code: 'k7qp m2xd-9wrt' } })
    assert.ok(r.container.textContent.includes('Pairing…'))
    assert.ok(!input(r.container, 'manager-code'), 'no code field while pairing')
    assert.ok(!r.container.innerHTML.toLowerCase().includes('k7qp'), 'the code is not kept on the page')
  } finally { await r.cleanup() }
})

test('a refused code shows the plain message, and the field again', async () => {
  const r = await render({ ...base, enabled: true, address: 'nas.local:10000', effective_address: 'nas.local:10000', state: 'pair_failed', message: WRONG },
    () => { throw new Error('unused') })
  try {
    assert.ok(r.container.textContent.includes('Pairing failed'))
    assert.ok(r.container.textContent.includes(WRONG))
    assert.ok(input(r.container, 'manager-code'))
  } finally { await r.cleanup() }
  // An invalid code: the server's message is shown.
  const bad = await render({ ...base, enabled: true, address: 'nas.local:10000', effective_address: 'nas.local:10000', state: 'needs_pairing', message: NOT_PAIRED })
  const oldFetch = globalThis.fetch
  try {
    globalThis.fetch = async (url, options) => options?.method === 'POST'
      ? Response.json({ error: "That isn't a valid pairing code" }, { status: 400 })
      : oldFetch(url, options)
    await type(input(bad.container, 'manager-code'), '1234')
    await act(async () => button(bad.container, 'Pair')!.click())
    assert.ok(bad.container.querySelector('[role="alert"]')!.textContent.includes("That isn't a valid pairing code"))
  } finally { globalThis.fetch = oldFetch; await bad.cleanup() }
})

test('without a web password it says so and links to where to set it', async () => {
  const r = await render({ ...base, enabled: true, web_password_set: false, state: 'needs_password', message: 'Set a web password first.' })
  try {
    const link = r.container.querySelector(`a[href="${WEB_PASSWORD_LINK}"]`)
    assert.ok(link, 'link to the web password step')
    assert.ok(r.container.textContent.includes('Set a web password first.'))
    assert.ok(!button(r.container, 'Pair'))
  } finally { await r.cleanup() }
})

test('a paired link shows the address and the fingerprint for reference, and unpair needs a confirm', async () => {
  const display = '61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27'
  const paired = { ...base, enabled: true, address: 'nas.local:10000', effective_address: 'nas.local:10000', paired: true, connected: true,
    state: 'connected' as const, message: 'Connected to Sentry Manager.', fingerprint: 'x', fingerprint_display: display }
  const r = await render(paired, () => ({ ...paired, paired: false, fingerprint: null, fingerprint_display: null, connected: false, state: 'needs_pairing', message: NOT_PAIRED }))
  try {
    assert.ok(r.container.textContent.includes('Paired with Sentry Manager at nas.local:10000'))
    assert.ok(r.container.textContent.includes(display))
    assert.ok(r.container.textContent.includes('for reference'))
    assert.ok(!input(r.container, 'manager-code'), 'no code field once paired')
    await act(async () => button(r.container, 'Unpair')!.click())
    assert.equal(r.writes.length, 0, 'first press only arms the confirm')
    await act(async () => button(r.container, 'Confirm unpair')!.click())
    assert.deepEqual(r.writes[0], { method: 'POST', url: '/api/manager-link/unpair', body: undefined })
    assert.ok(!r.container.textContent.includes(display))
    assert.ok(input(r.container, 'manager-code'), 'the code field is back')
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
