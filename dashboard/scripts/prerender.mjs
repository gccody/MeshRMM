// Writes each page's HTML into dist/: the client build's index.html with the
// page rendered into it. The server serves `/` from dist/index.html,
// `/<page>` from dist/<page>/index.html, and anything else from dist/404.html.
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "dist");
const ssr = join(root, ".prerender");

const { render, PAGES, NOT_FOUND_PATH, NOT_FOUND_TITLE } = await import(pathToFileURL(join(ssr, "entry-server.js")).href);
const template = await readFile(join(dist, "index.html"), "utf8");
if (!template.includes("<!--page-html-->") || !template.includes("<!--page-title-->MeshRMM")) {
  throw new Error("dist/index.html is missing the page placeholders");
}

const escapeHtml = (text) => text.replace(/[&<>"']/g, (character) => `&#${character.charCodeAt(0)};`);

function page(path, title) {
  return template
    .replace("<!--page-title-->MeshRMM", `${escapeHtml(title)} · MeshRMM`)
    .replace("<!--page-html-->", () => render(path));
}

const files = [
  ...PAGES.map(({ path, title }) => [path === "/" ? "index.html" : join(path.slice(1), "index.html"), page(path, title)]),
  ["404.html", page(NOT_FOUND_PATH, NOT_FOUND_TITLE)],
];
for (const [file, html] of files) {
  const target = join(dist, file);
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, html);
}
await rm(ssr, { recursive: true, force: true });
console.log(`prerendered ${files.length} pages`);
