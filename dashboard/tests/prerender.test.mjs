// Checks the pages `npm run build` prerenders into dist/, which the server
// embeds. Run after a build (`npm test` builds first).
import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";
import { NOT_FOUND_TITLE, PAGES } from "../src/pages.ts";

const dist = new URL("../dist/", import.meta.url);
const page = (path) => readFile(new URL(path === "/" ? "index.html" : `${path.slice(1)}/index.html`, dist), "utf8");

test("every page is prerendered with its title", async () => {
  for (const { path, title } of PAGES) {
    const html = await page(path);
    assert.ok(html.includes(`<title>${title} · MeshRMM</title>`), path);
    assert.ok(html.includes('<div id="root"><div class="app-root">'), `${path} has its rendered app`);
  }
  const notFound = await readFile(new URL("404.html", dist), "utf8");
  assert.ok(notFound.includes(`<title>${NOT_FOUND_TITLE} · MeshRMM</title>`));
  assert.ok(notFound.includes(`<h1 id="auth-title">${NOT_FOUND_TITLE}</h1>`));
});

// The server's Content-Security-Policy is default-src 'self' with no
// 'unsafe-inline', so prerendered HTML may not carry inline scripts or styles.
test("pages need nothing the Content-Security-Policy forbids", async () => {
  for (const path of [...PAGES.map((entry) => entry.path), "/404"]) {
    const html = path === "/404" ? await readFile(new URL("404.html", dist), "utf8") : await page(path);
    const scripts = [...html.matchAll(/<script\b[^>]*>/g)].map(([tag]) => tag);
    assert.deepEqual(scripts.filter((tag) => !/\ssrc="\/assets\/[^"]+\.js"/.test(tag)), [], `${path} has only external scripts`);
    assert.equal(scripts.length, 1, path);
    assert.doesNotMatch(html, /\sstyle="/, `${path} has no style attributes`);
    assert.doesNotMatch(html, /<style\b/, `${path} has no style elements`);
    assert.doesNotMatch(html, /\s(?:src|href|action)="(?:https?:)?\/\//, `${path} loads nothing from another origin`);
  }
});

test("workspace pages show their heading over a loading state until the session is known", async () => {
  for (const [path, heading] of [["/", "Devices"], ["/users", "Users"], ["/audit", "Audit log"], ["/account", "Your account"]]) {
    const html = await page(path);
    assert.ok(html.includes(`<h1>${heading}</h1>`), path);
    assert.ok(html.includes("Loading…"), path);
    assert.ok(html.includes("Checking session"), path);
    // Nothing that depends on who is signed in.
    assert.doesNotMatch(html, /class="nav-item/, path);
  }
});

test("the sign-in page is a working form before any script runs", async () => {
  const html = await page("/login");
  assert.match(html, /<input id="email" type="email" autoComplete="username"|<input id="email" type="email" autocomplete="username"/);
  assert.match(html, /<input id="password" type="password"/);
  assert.ok(html.includes('href="/reset"'));
});

test("the build carries no hosted-service code and stays small", async () => {
  const assets = await readdir(new URL("assets/", dist));
  const scripts = assets.filter((name) => name.endsWith(".js"));
  assert.equal(scripts.length, 1, "one bundle, so every page hydrates without waiting on chunks");
  const bundle = await readFile(new URL(`assets/${scripts[0]}`, dist), "utf8");
  for (const name of ["workos", "radix-ui", "cloudflare", "meshrmm.com"]) {
    assert.doesNotMatch(bundle, new RegExp(name, "i"), name);
  }
  assert.ok(bundle.length < 600_000, `the bundle is ${bundle.length} bytes`);
});
