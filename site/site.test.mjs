import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile, readdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const SITE = dirname(fileURLToPath(import.meta.url));
// The only places the site links out to. Nothing is loaded from them.
const ALLOWED_ORIGINS = new Set(["https://github.com", "https://meshrmm.com"]);

const pages = (await readdir(SITE)).filter((name) => name.endsWith(".html"));
const css = await readFile(join(SITE, "styles.css"), "utf8");

function attributes(html, name) {
  return [...html.matchAll(new RegExp(`\\s${name}="([^"]*)"`, "g"))].map((match) => match[1]);
}

for (const page of pages) {
  const html = await readFile(join(SITE, page), "utf8");

  test(`${page} runs no scripts`, () => {
    assert.doesNotMatch(html, /<script\b/i);
    assert.doesNotMatch(html, /\son[a-z]+="/i);
  });

  test(`${page} has a title, description and language`, () => {
    assert.match(html, /<html lang="en">/);
    assert.match(html, /<title>[^<]+<\/title>/);
    assert.match(html, /<meta name="description" content="[^"]+">/);
  });

  test(`${page} has unique ids and every in-page link has a target`, () => {
    const ids = attributes(html, "id");
    assert.deepEqual(ids, [...new Set(ids)]);
    for (const reference of attributes(html, "href").filter((href) => href.startsWith("#"))) {
      assert.ok(ids.includes(reference.slice(1)), `${reference} has no target`);
    }
    for (const reference of attributes(html, "aria-labelledby")) {
      assert.ok(ids.includes(reference), `aria-labelledby="${reference}" has no target`);
    }
  });

  test(`${page} links only to local files and the allowed sites`, () => {
    for (const reference of [...attributes(html, "href"), ...attributes(html, "src")]) {
      if (reference.startsWith("#")) continue;
      if (/^[a-z]+:/i.test(reference)) {
        const url = new URL(reference);
        assert.ok(ALLOWED_ORIGINS.has(url.origin), `${reference} is not an allowed site`);
        continue;
      }
      const path = reference.split(/[?#]/)[0] || "index.html";
      const file = path.endsWith("/") ? `${path}index.html` : path;
      assert.ok(existsSync(join(SITE, file)), `${reference} does not exist`);
    }
  });

  test(`${page} loads nothing from other sites`, () => {
    const loads = [
      ...html.matchAll(/<(?:link|img|source|iframe|video|audio)\b[^>]*\s(?:href|src)="([^"]*)"/g),
    ].map((match) => match[1]);
    for (const reference of loads) {
      const external = /^[a-z]+:/i.test(reference) || reference.startsWith("//");
      // The canonical link names the site's own address; it isn't fetched.
      if (external && html.includes(`<link rel="canonical" href="${reference}">`)) continue;
      assert.ok(!external, `${reference} is loaded from another site`);
    }
  });
}

test("styles.css loads only local files", () => {
  const references = [...css.matchAll(/url\("?([^")]+)"?\)/g)].map((match) => match[1]);
  assert.ok(references.length > 0);
  for (const reference of references) {
    assert.doesNotMatch(reference, /^([a-z]+:|\/\/)/i, `${reference} is not local`);
    assert.ok(existsSync(join(SITE, reference)), `${reference} does not exist`);
  }
  assert.doesNotMatch(css, /@import\b/);
});
