import assert from 'node:assert/strict';
import test from 'node:test';
import { AuthenticationRequired } from '../lib/http.ts';
import { RECENT_THUMBNAILS, THUMBNAIL_INTERVAL_MS, THUMBNAIL_REQUESTS, ThumbnailStore, nextCheckAt } from '../features/agents/thumbnails.ts';

const MINUTE = 60_000;

function harness(responses) {
  const requests = [];
  const signals = [];
  const created = [];
  const revoked = [];
  let clock = 1_000_000_000;
  const store = new ThumbnailStore({
    fetch: async (path, init) => {
      requests.push({ path, ifNoneMatch: init.headers['If-None-Match'] ?? null, cache: init.cache });
      signals.push(init.signal);
      const next = responses.shift();
      if (next instanceof Error) throw next;
      return typeof next === 'function' ? next(init.signal) : next;
    },
    createUrl: (blob) => { const url = `blob:${created.length}`; created.push({ url, blob }); return url; },
    revokeUrl: (url) => revoked.push(url),
    now: () => clock,
  });
  return { store, requests, signals, created, revoked, advance: (ms) => { clock += ms; }, now: () => clock };
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


const settle = () => new Promise((resolve) => setImmediate(resolve));

// A response that comes when the test says, or an abort error, like a real
// fetch, once its signal is aborted.
function later() {
  const pending = [];
  const respond = (signal) => new Promise((resolve, reject) => {
    pending.push(resolve);
    signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
  });
  return { respond, pending };
}

test('only the most recently hidden images are kept, and every other one is released once', async () => {
  const count = RECENT_THUMBNAILS + 50;
  const h = harness(Array.from({ length: count + 2 }, (_, index) => image(`"${index}"`, 1_000_000_000)));
  const shown = h.store.subscribe('pc-0', () => {});
  await h.store.refresh('pc-0', true);
  for (let index = 1; index < count; index++) {
    const unsubscribe = h.store.subscribe(`pc-${index}`, () => {});
    await h.store.refresh(`pc-${index}`, true);
    unsubscribe();
  }
  const kept = Array.from({ length: count }, (_, index) => h.store.snapshot(`pc-${index}`)).filter(Boolean);
  assert.equal(kept.length, RECENT_THUMBNAILS + 1, 'the shown tile and the most recently hidden ones');
  assert.equal(h.store.snapshot('pc-0').url, 'blob:0', 'a tile still shown keeps its image');
  assert.equal(h.store.snapshot('pc-1'), null, 'the longest hidden is forgotten');
  assert.equal(h.store.snapshot(`pc-${count - 1}`).url, `blob:${count - 1}`);
  const dropped = Array.from({ length: count - 1 - RECENT_THUMBNAILS }, (_, index) => `blob:${index + 1}`);
  assert.deepEqual(h.revoked, dropped, 'every forgotten image is released, once');

  // A recent one is drawn at once and not loaded again.
  const back = h.store.subscribe(`pc-${count - 1}`, () => {});
  assert.equal(await h.store.refresh(`pc-${count - 1}`, true), THUMBNAIL_INTERVAL_MS);
  assert.equal(h.requests.length, count);
  back();

  // A forgotten one loads again, from the start: its ETag would bring no image.
  const again = h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1', true);
  assert.equal(h.requests.length, count + 1);
  assert.equal(h.requests[count].ifNoneMatch, null);
  assert.equal(h.store.snapshot('pc-1').url, `blob:${count}`);
  again();
  shown();

  h.store.clear();
  assert.equal(new Set(h.revoked).size, h.revoked.length, 'no image is released twice');
  assert.equal(h.revoked.length, h.created.length, 'and none is left');
});

test("a hidden tile's load is stopped, and its response publishes nothing", async () => {
  let release;
  const h = harness([
    () => new Promise((resolve) => { release = () => resolve(image('"v1"', 1_000_000_000)); }),
    image('"v2"', 1_000_000_000, 'second'),
  ]);
  const heard = [];
  const unsubscribe = h.store.subscribe('pc-1', () => heard.push('pc-1'));
  const loading = h.store.refresh('pc-1', true);
  await settle();
  assert.equal(h.signals[0].aborted, false);
  unsubscribe();
  assert.equal(h.signals[0].aborted, true, 'the request is aborted');
  release();
  assert.equal(await loading, null);
  assert.equal(h.store.snapshot('pc-1'), null, 'a response that ignored the abort is dropped');
  assert.equal(h.created.length, 0, 'and makes no object URL');
  assert.deepEqual(heard, []);

  h.store.subscribe('pc-1', () => {});
  assert.notEqual(await h.store.refresh('pc-1', true), null, 'shown again, it loads');
  assert.equal(h.requests.length, 2);
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.deepEqual(h.revoked, []);
});

test('a tile shown again while its stopped load ends loads again', async () => {
  const { respond, pending } = later();
  const h = harness([respond, image('"v1"', 1_000_000_000)]);
  const unsubscribe = h.store.subscribe('pc-1', () => {});
  const stopped = h.store.refresh('pc-1', true);
  await settle();
  unsubscribe();
  h.store.subscribe('pc-1', () => {});
  const shown = h.store.refresh('pc-1', true);
  await stopped;
  assert.notEqual(await shown, null);
  assert.equal(h.requests.length, 2);
  assert.equal(pending.length, 1);
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
});

test('a few images load at once, and a hidden tile waiting for its turn asks for nothing', async () => {
  const { respond, pending } = later();
  const count = THUMBNAIL_REQUESTS + 3;
  const h = harness(Array.from({ length: count }, () => respond));
  const unsubscribe = [];
  const loads = [];
  for (let index = 0; index < count; index++) {
    unsubscribe.push(h.store.subscribe(`pc-${index}`, () => {}));
    loads.push(h.store.refresh(`pc-${index}`, true));
  }
  await settle();
  assert.equal(h.requests.length, THUMBNAIL_REQUESTS);

  // Hidden while waiting: never sent.
  unsubscribe[THUMBNAIL_REQUESTS]();
  // Hidden while loading: its request is aborted and frees its place.
  unsubscribe[0]();
  await settle();
  assert.deepEqual(h.requests.map((request) => request.path).slice(THUMBNAIL_REQUESTS), [`/v1/agents/pc-${THUMBNAIL_REQUESTS + 1}/thumbnail`]);

  pending[1](image('"v1"', 1_000_000_000));
  await settle();
  assert.equal(h.requests.at(-1).path, `/v1/agents/pc-${THUMBNAIL_REQUESTS + 2}/thumbnail`);
  assert.equal(h.requests.length, THUMBNAIL_REQUESTS + 2);
  assert.ok(!h.requests.some((request) => request.path === `/v1/agents/pc-${THUMBNAIL_REQUESTS}/thumbnail`));

  for (const resolve of pending.slice(2)) resolve(image('"v1"', 1_000_000_000));
  await Promise.all(loads);
  assert.equal(h.store.snapshot('pc-0'), null);
  assert.equal(h.store.snapshot(`pc-${THUMBNAIL_REQUESTS}`), null);
  assert.equal(h.created.length, count - 2);
});
