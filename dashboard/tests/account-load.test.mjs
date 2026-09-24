import assert from 'node:assert/strict';
import test from 'node:test';
import {
  AccountLoadError,
  accountLoader,
  accountRetryDelay,
  isRetryableAccountError,
} from '../features/workspace/account-load.ts';

const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };

test('outages, rate limits and network failures are retried; other failures are not', () => {
  for (const status of [408, 429, 500, 502, 503]) assert.equal(isRetryableAccountError(new AccountLoadError('x', status)), true);
  for (const status of [400, 403, 404, 409]) assert.equal(isRetryableAccountError(new AccountLoadError('x', status)), false);
  assert.equal(isRetryableAccountError(new TypeError('Failed to fetch')), true);
  assert.equal(isRetryableAccountError(new SyntaxError('Unexpected token')), false);
  assert.deepEqual([0, 1, 2, 3, 4, 5, 6].map(accountRetryDelay), [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
});

test('a 503 is reported and retried with backoff until the account loads', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const results = [new AccountLoadError('Service unavailable', 503), new TypeError('Failed to fetch'), { company: 'acme' }];
  let calls = 0;
  const errors = [];
  const loaded = [];
  accountLoader({
    load: async () => { const result = results[calls++]; if (result instanceof Error) throw result; return result; },
    onLoaded: value => loaded.push(value),
    onError: (error, retryInMs) => errors.push([error.message, retryInMs]),
  });
  await flush();
  assert.deepEqual(errors, [['Service unavailable', 1_000]]);
  t.mock.timers.tick(999);
  await flush();
  assert.equal(calls, 1);
  t.mock.timers.tick(1);
  await flush();
  assert.deepEqual(errors[1], ['Failed to fetch', 2_000]);
  t.mock.timers.tick(2_000);
  await flush();
  assert.equal(calls, 3);
  assert.deepEqual(loaded, [{ company: 'acme' }]);
});

test('a failure that needs action waits for a manual retry', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let calls = 0;
  const errors = [];
  const loader = accountLoader({
    load: async () => { calls++; throw new AccountLoadError('Company suspended', 403); },
    onLoaded: () => assert.fail('unexpected load'),
    onError: (error, retryInMs) => errors.push(retryInMs),
  });
  await flush();
  t.mock.timers.tick(60_000);
  await flush();
  assert.equal(calls, 1);
  assert.deepEqual(errors, [null]);
  loader.retry();
  await flush();
  assert.equal(calls, 2);
});

test('a manual retry resets the backoff and does not overlap a load', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let calls = 0;
  const delays = [];
  let release;
  const loader = accountLoader({
    load: () => { calls++; return calls === 3 ? new Promise((_, reject) => { release = reject; }) : Promise.reject(new AccountLoadError('down', 503)); },
    onLoaded: () => {},
    onError: (error, retryInMs) => delays.push(retryInMs),
  });
  await flush();
  t.mock.timers.tick(1_000);
  await flush();
  assert.deepEqual(delays, [1_000, 2_000]);
  loader.retry();
  await flush();
  assert.equal(calls, 3);
  loader.retry();
  await flush();
  assert.equal(calls, 3, 'no second request while one is in flight');
  release(new AccountLoadError('down', 503));
  await flush();
  assert.deepEqual(delays, [1_000, 2_000, 1_000]);
  t.mock.timers.tick(2_000);
  await flush();
  assert.equal(calls, 4, 'the pending automatic retry was replaced');
});

test('stopping cancels retries and ignores a late result', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let calls = 0;
  let resolve;
  const loader = accountLoader({
    load: () => { calls++; return calls === 1 ? Promise.reject(new AccountLoadError('down', 503)) : new Promise(r => { resolve = r; }); },
    onLoaded: () => assert.fail('late result applied'),
    onError: () => {},
  });
  await flush();
  t.mock.timers.tick(1_000);
  await flush();
  loader.stop();
  resolve({});
  t.mock.timers.tick(60_000);
  await flush();
  assert.equal(calls, 2);
});
