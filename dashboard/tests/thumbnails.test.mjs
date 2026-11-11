import assert from 'node:assert/strict';
import test from 'node:test';
import { AuthenticationRequired } from '../lib/http.ts';
import { RECENT_THUMBNAILS, THUMBNAIL_REQUESTS, ThumbnailStore } from '../features/agents/thumbnails.ts';

const MINUTE = 60_000;
const UPLOADED = 1_000_000_000;

function harness(responses) {
  const requests = [];
  const signals = [];
  const created = [];
  const revoked = [];
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
  });
  return { store, requests, signals, created, revoked };
}

const image = (etag, lastModified, bytes = 'jpeg') => new Response(bytes, {
  headers: { ETag: etag, 'Last-Modified': new Date(lastModified).toUTCString(), 'Content-Type': 'image/jpeg' },
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

test('a shown device is loaded once, and not polled', async () => {
  const h = harness([image('"v1"', UPLOADED)]);
  h.store.subscribe('pc-1', () => {});
  assert.equal(await h.store.refresh('pc-1'), false);
  assert.deepEqual(h.requests[0], { path: '/v1/agents/pc-1/thumbnail', ifNoneMatch: null, cache: 'no-store' });
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.equal(h.store.snapshot('pc-1').updatedAt.getTime(), UPLOADED);
  assert.equal(await h.created[0].blob.text(), 'jpeg');
  // The tile checking again, as when the tab is shown, loads nothing: the
  // socket says when there is a new image.
  for (let check = 0; check < 3; check++) assert.equal(await h.store.refresh('pc-1'), false);
  assert.equal(h.requests.length, 1);
});

test('a replaced image is loaded at once with its ETag and shown', async () => {
  const h = harness([image('"v1"', UPLOADED - 50 * MINUTE), image('"v2"', UPLOADED, 'newer'), new Response(null, { status: 304 })]);
  const updates = [];
  h.store.subscribe('pc-1', () => updates.push(h.store.snapshot('pc-1')));
  await h.store.refresh('pc-1');
  // The Agent refreshed it seconds after the tile was shown.
  h.store.replaced(['pc-1', 'not-shown']);
  await h.store.refresh('pc-1');
  assert.equal(h.requests.length, 2, 'a device the page never showed loads nothing');
  assert.equal(h.requests[1].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
  assert.equal(await h.created[1].blob.text(), 'newer');
  assert.equal(h.store.snapshot('pc-1').updatedAt.getTime(), UPLOADED, 'the preview says when the new one arrived');
  assert.deepEqual(h.revoked, ['blob:0'], 'the replaced image is released');
  assert.equal(updates.length, 2);
  // Told again, as when a socket fell behind: a 304 keeps it.
  h.store.replaced(['pc-1']);
  await h.store.refresh('pc-1');
  assert.equal(h.requests[2].ifNoneMatch, '"v2"');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
  assert.equal(updates.length, 2);
});

test('an image replaced while it loads is loaded again after', async () => {
  const { respond, pending } = later();
  const h = harness([respond, image('"v2"', UPLOADED, 'newer')]);
  h.store.subscribe('pc-1', () => {});
  const loading = h.store.refresh('pc-1');
  await settle();
  h.store.replaced(['pc-1']);
  await settle();
  assert.equal(h.requests.length, 1, 'one request at a time');
  pending[0](image('"v1"', UPLOADED - MINUTE));
  await loading;
  await h.store.refresh('pc-1');
  assert.equal(h.requests.length, 2);
  assert.equal(h.requests[1].ifNoneMatch, '"v1"');
  assert.equal(await h.created.at(-1).blob.text(), 'newer');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
});

test('an image replaced while no row shows it loads when it is shown again', async () => {
  const h = harness([image('"v1"', UPLOADED), image('"v2"', UPLOADED + MINUTE)]);
  const hide = h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1');
  hide();
  h.store.replaced(['pc-1']);
  await settle();
  assert.equal(h.requests.length, 1);
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0', 'the last image stays to draw at once');
  h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1');
  assert.equal(h.requests[1].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
});

test('devices the socket starts watching again, as after a reconnect, are checked once', async () => {
  const { respond, pending } = later();
  const h = harness([image('"a"', UPLOADED), image('"b"', UPLOADED), respond, new Response(null, { status: 304 }), image('"b2"', UPLOADED + MINUTE)]);
  for (const id of ['pc-a', 'pc-b', 'pc-c']) h.store.subscribe(id, () => {});
  await h.store.refresh('pc-a');
  await h.store.refresh('pc-b');
  // pc-c is still on its first load, which shows what is there now.
  const first = h.store.refresh('pc-c');
  await settle();
  assert.equal(h.requests.length, 3);
  h.store.revalidate(['pc-a', 'pc-b', 'pc-c', 'unknown']);
  await h.store.refresh('pc-a');
  await h.store.refresh('pc-b');
  assert.deepEqual(h.requests.slice(3).map((request) => [request.path, request.ifNoneMatch]), [
    ['/v1/agents/pc-a/thumbnail', '"a"'],
    ['/v1/agents/pc-b/thumbnail', '"b"'],
  ]);
  assert.equal(h.store.snapshot('pc-a').url, 'blob:0', 'unchanged');
  assert.equal(h.store.snapshot('pc-b').url, 'blob:2', 'what the old socket missed');
  pending[0](image('"c"', UPLOADED));
  await first;
  assert.equal(h.requests.length, 5, 'a first load is not repeated');
});

test('a missing image clears the row, and failures keep the last image and ask to try again', async () => {
  const h = harness([image('"v1"', UPLOADED), new Response('{}', { status: 503 }), new Error('offline'), new Response(null, { status: 204 })]);
  h.store.subscribe('pc-1', () => {});
  assert.equal(await h.store.refresh('pc-1'), false);
  h.store.replaced(['pc-1']);
  assert.equal(await h.store.refresh('pc-1'), true, 'a server error is tried again');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.equal(await h.store.refresh('pc-1'), true, 'so is a network failure');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.equal(await h.store.refresh('pc-1'), false);
  assert.equal(h.requests[3].ifNoneMatch, '"v1"');
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
  assert.equal(await h.store.refresh('pc-1'), false, 'no image is a checked image');
  assert.equal(h.requests.length, 4);
});

test('concurrent rows share one request', async () => {
  let release;
  const h = harness([() => new Promise((resolve) => { release = () => resolve(image('"v1"', UPLOADED)); })]);
  const first = h.store.refresh('pc-1');
  const second = h.store.refresh('pc-1');
  await settle();
  release();
  assert.equal(await first, await second);
  assert.equal(h.requests.length, 1);
});

test('clearing discards images, including one still loading', async () => {
  let release;
  const h = harness([image('"v1"', UPLOADED), () => new Promise((resolve) => { release = () => resolve(image('"v2"', UPLOADED)); }), image('"v3"', UPLOADED)]);
  h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1');
  h.store.replaced(['pc-1']);
  const loading = h.store.refresh('pc-1');
  await settle();
  h.store.clear();
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
  release();
  assert.equal(await loading, false, 'nothing to try again');
  assert.equal(h.store.snapshot('pc-1'), null, 'a response from before the lock is dropped');
  assert.equal(h.created.length, 1);
  await h.store.refresh('pc-1');
  assert.equal(h.requests[2].ifNoneMatch, null, 'the next load starts over');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:1');
});

test('a locked session stops loading', async () => {
  const h = harness([new AuthenticationRequired()]);
  assert.equal(await h.store.refresh('pc-1'), false, 'nothing is tried again');
  assert.equal(h.store.snapshot('pc-1'), null);
});

test('devices that left the inventory are forgotten unless a row still shows them', async () => {
  const h = harness([image('"a"', UPLOADED), image('"b"', UPLOADED)]);
  await h.store.refresh('pc-1');
  await h.store.refresh('pc-2');
  const unsubscribe = h.store.subscribe('pc-2', () => {});
  h.store.retain(new Set());
  assert.equal(h.store.snapshot('pc-1'), null);
  assert.deepEqual(h.revoked, ['blob:0']);
  assert.equal(h.store.snapshot('pc-2').url, 'blob:1');
  unsubscribe();
  h.store.retain(new Set());
  assert.deepEqual(h.revoked, ['blob:0', 'blob:1']);
});

test('only the most recently hidden images are kept, and every other one is released once', async () => {
  const count = RECENT_THUMBNAILS + 50;
  const h = harness(Array.from({ length: count + 2 }, (_, index) => image(`"${index}"`, UPLOADED)));
  const shown = h.store.subscribe('pc-0', () => {});
  await h.store.refresh('pc-0');
  for (let index = 1; index < count; index++) {
    const unsubscribe = h.store.subscribe(`pc-${index}`, () => {});
    await h.store.refresh(`pc-${index}`);
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
  await h.store.refresh(`pc-${count - 1}`);
  assert.equal(h.requests.length, count);
  back();

  // A forgotten one loads again, from the start: its ETag would bring no image.
  const again = h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1');
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
    () => new Promise((resolve) => { release = () => resolve(image('"v1"', UPLOADED)); }),
    image('"v2"', UPLOADED, 'second'),
  ]);
  const heard = [];
  const unsubscribe = h.store.subscribe('pc-1', () => heard.push('pc-1'));
  const loading = h.store.refresh('pc-1');
  await settle();
  assert.equal(h.signals[0].aborted, false);
  unsubscribe();
  assert.equal(h.signals[0].aborted, true, 'the request is aborted');
  release();
  assert.equal(await loading, false);
  assert.equal(h.store.snapshot('pc-1'), null, 'a response that ignored the abort is dropped');
  assert.equal(h.created.length, 0, 'and makes no object URL');
  assert.deepEqual(heard, []);

  h.store.subscribe('pc-1', () => {});
  await h.store.refresh('pc-1');
  assert.equal(h.requests.length, 2, 'shown again, it loads');
  assert.equal(h.store.snapshot('pc-1').url, 'blob:0');
  assert.deepEqual(h.revoked, []);
});

test('a tile shown again while its stopped load ends loads again', async () => {
  const { respond, pending } = later();
  const h = harness([respond, image('"v1"', UPLOADED)]);
  const unsubscribe = h.store.subscribe('pc-1', () => {});
  const stopped = h.store.refresh('pc-1');
  await settle();
  unsubscribe();
  h.store.subscribe('pc-1', () => {});
  const shown = h.store.refresh('pc-1');
  await stopped;
  await shown;
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
    loads.push(h.store.refresh(`pc-${index}`));
  }
  await settle();
  assert.equal(h.requests.length, THUMBNAIL_REQUESTS);

  // Hidden while waiting: never sent.
  unsubscribe[THUMBNAIL_REQUESTS]();
  // Hidden while loading: its request is aborted and frees its place.
  unsubscribe[0]();
  await settle();
  assert.deepEqual(h.requests.map((request) => request.path).slice(THUMBNAIL_REQUESTS), [`/v1/agents/pc-${THUMBNAIL_REQUESTS + 1}/thumbnail`]);

  pending[1](image('"v1"', UPLOADED));
  await settle();
  assert.equal(h.requests.at(-1).path, `/v1/agents/pc-${THUMBNAIL_REQUESTS + 2}/thumbnail`);
  assert.equal(h.requests.length, THUMBNAIL_REQUESTS + 2);
  assert.ok(!h.requests.some((request) => request.path === `/v1/agents/pc-${THUMBNAIL_REQUESTS}/thumbnail`));

  for (const resolve of pending.slice(2)) resolve(image('"v1"', UPLOADED));
  await Promise.all(loads);
  assert.equal(h.store.snapshot('pc-0'), null);
  assert.equal(h.store.snapshot(`pc-${THUMBNAIL_REQUESTS}`), null);
  assert.equal(h.created.length, count - 2);
});
