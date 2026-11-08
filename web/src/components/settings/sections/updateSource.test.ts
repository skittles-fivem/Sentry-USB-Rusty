import assert from 'node:assert/strict'
import test from 'node:test'
import { customRepo, isTestBuild, isValidRepo } from './updateSource.ts'

test('custom update repositories must be a GitHub owner/name', () => {
  for (const ok of ['skittles-fivem/Sentry-USB-Rusty-testing', 'Sentry-Six/Sentry-USB-Rusty', 'a/b_c.d', ' a/b ']) {
    assert.ok(isValidRepo(ok), ok)
  }
  for (const bad of ['', 'owner', 'owner/', '/name', 'a/b/c', '-a/b', 'a-/b', 'a_b/c', 'a/..', 'a/.', 'a/b c', 'https://github.com/a/b']) {
    assert.ok(!isValidRepo(bad), bad)
  }
})

test('test builds and custom sources are labelled, official is not', () => {
  assert.ok(isTestBuild('v4.98.1-test.2'))
  assert.ok(!isTestBuild('v4.98.1'))
  assert.ok(!isTestBuild('v4.98.1-beta.1'))
  assert.equal(customRepo('Sentry-Six/Sentry-USB-Rusty'), null)
  assert.equal(customRepo('sentry-six/sentry-usb-rusty'), null)
  assert.equal(customRepo(null), null)
  assert.equal(customRepo('skittles-fivem/Sentry-USB-Rusty-testing'), 'skittles-fivem/Sentry-USB-Rusty-testing')
})
