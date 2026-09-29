import assert from 'node:assert/strict';
import test from 'node:test';
import { AuthenticationRequired } from '../lib/http.ts';
import { THUMBNAIL_INTERVAL_MS, ThumbnailStore, nextCheckAt } from '../features/agents/thumbnails.ts';

const MINUTE = 60_000;

function harness(responses) {
  const requests = [];
  const created = [];
  const revoked = [];
  let clock = 1_000_000_000;
  const store = new ThumbnailStore({
    fetch: async (path, init) => {
      requests.push({ path, ifNoneMatch: init.headers['If-None-Match'] ?? null, cache: init.cache });
      const next = responses.shift();
      if (next instanceof Error) throw next;
      return typeof next === 'function' ? next() : next;
    },
    createUrl: (blob) => { const url = `blob:${created.length}`; created.push({ url, blob }); return url; },
    revokeUrl: (url) => revoked.push(url),
    now: () => clock,
  });
  return { store, requests, created, revoked, advance: (ms) => { clock += ms; }, now: () => clock };
}

const image = (etag, lastModified, bytes = 'jpeg') => new Response(bytes, {
  headers: { ETag: etag, 'Last-Modified': new Date(lastModified).toUTCString(), 'Content-Type': 'image/jpeg' },
});

test('checks just after the Agent uploads, and a full interval later when the screen is unchanged', () => {
  const now = 10 * MINUTE;
  assert.equal(nextCheckAt(now, now - MINUTE), now + 4 * MINUTE + 20_000);
  assert.equal(nextCheckAt(now, now - 3 * MINUTE), now + 2 * MINUTE + 20_000);
  assert.equal(nextCheckAt(now, now - 6 * MINUTE), now + THUMBNAIL_INTERVAL_MS, 'an old image waits a full interval');
  assert.equal(nextCheckAt(now, now - 5 * MINUTE - 15_000), now + 10_000, 'checks are at least ten seconds apart');
  assert.equal(nextCheckAt(now, now + 60 * MINUTE), now + THUMBNAIL_INTERVAL_MS, "a server clock ahead of the browser's waits at most an interval");
  assert.equal(nextCheckAt(now, null), now + THUMBNAIL_INTERVAL_MS);
});

test('revalidates with the ETag and keeps the image when it is unchanged', async () => {
  const uploaded = 1_000_000_000 - 2 * MINUTE;
  const h = harness([image('"v1"', uploaded), new Response(null, { status: 304 }), image('"v2"', uploaded + 5 * MINUTE, 'newer')]);
  const updates = [];
  h.store.subscribe('pc-1', () => updates.push(h.store.snapshot('pc-1')));

  const delay = await h.store.refresh('pc-1', true);
  assert.deepEqual(h.requests[0], { path: '/v1/agents/pc-1/thumbnail', ifNoneMatch: null, cache: 'no-store' });
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.equal(h.store.snapshot('pc-1').updatedAt.getTime(), uploaded);
  assert.equal(await h.created[0].blob.text(), 'jpeg');
  assert.equal(delay, 3 * MINUTE + 20_000, 'the next check follows the next upload');

  assert.equal(await h.store.refresh('pc-1', true), delay, 'a check that is not due fetches nothing');
  assert.equal(h.requests.length, 1);

  h.advance(delay);
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests[1].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0', 'a 304 keeps the image');
  assert.equal(updates.length, 1);

  h.advance(THUMBNAIL_INTERVAL_MS);
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests[2].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
  assert.deepEqual(h.revoked, ['blob:0'], 'the replaced image is released');
  assert.equal(updates.length, 2);
});

test('an offline device is loaded once, then checked soon after it comes online', async () => {
  const h = harness([image('"v1"', 0), image('"v2"', 1_000_000_000 + 20_000)]);
  assert.equal(await h.store.refresh('pc-1', false), null, 'offline devices are not polled');
  assert.equal(await h.store.refresh('pc-1', false), null);
  assert.equal(h.requests.length, 1);
  assert.equal(await h.store.refresh('pc-1', true), 20_000, 'the Agent uploads when it connects');
  h.advance(20_000);
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests.length, 2);
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
});

test('concurrent rows share one request', async () => {
  let release;
  const h = harness([() => new Promise((resolve) => { release = () => resolve(image('"v1"', 1_000_000_000)); })]);
  const first = h.store.refresh('pc-1', true);
  const second = h.store.refresh('pc-1', true);
  await new Promise((resolve) => setImmediate(resolve));
  release();
  assert.equal(await first, await second);
  assert.equal(h.requests.length, 1);
});

test('a missing image clears the row, and failures keep the last image', async () => {
  const h = harness([image('"v1"', 1_000_000_000), new Response('{}', { status: 503 }), new Error('offline'), new Response(null, { status: 204 })]);
  await h.store.refresh('pc-1', true);
  for (let attempt = 0; attempt < 2; attempt++) {
    h.advance(THUMBNAIL_INTERVAL_MS);
    assert.equal(await h.store.refresh('pc-1', true), THUMBNAIL_INTERVAL_MS, 'a failure retries after an interval');
    assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  }
  h.advance(THUMBNAIL_INTERVAL_MS);
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests[3].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
});

test('clearing discards images, including one still loading', async () => {
  let release;
  const h = harness([image('"v1"', 1_000_000_000), () => new Promise((resolve) => { release = () => resolve(image('"v2"', 1_000_000_000)); }), image('"v3"', 1_000_000_000)]);
  await h.store.refresh('pc-1', true);
  h.advance(THUMBNAIL_INTERVAL_MS);
  const loading = h.store.refresh('pc-1', true);
  await new Promise((resolve) => setImmediate(resolve));
  h.store.clear();
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
  release();
  assert.equal(await loading, null, 'the row stops checking until it is shown again');
  assert.equal(h.store.snapshot('pc-1'), null, 'a response from before the lock is dropped');
  assert.equal(h.created.length, 1);
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests[2].ifNoneMatch, null, 'the next load starts over');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
});

test('a locked session stops loading', async () => {
  const h = harness([new AuthenticationRequired()]);
  assert.equal(await h.store.refresh('pc-1', true), null, 'no further check is scheduled');
  assert.equal(h.store.snapshot('pc-1'), null);
});

test('devices that left the inventory are forgotten unless a row still shows them', async () => {
  const h = harness([image('"a"', 1_000_000_000), image('"b"', 1_000_000_000)]);
  await h.store.refresh('pc-1', true);
  await h.store.refresh('pc-2', true);
  const unsubscribe = h.store.subscribe('pc-2', () => {});
  h.store.retain(new Set());
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
  assert.equal(h.store.snapshot('pc-2').url, 'blob:1');
  unsubscribe();
  h.store.retain(new Set());
  assert.deepEqual(h.revoked, ['blob:0', 'blob:1']);
});
