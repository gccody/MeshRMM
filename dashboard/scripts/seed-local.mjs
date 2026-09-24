// Prepares the local D1 database that `npm run dev` shares with the control
// plane: applies the server's migrations, then adds or updates one active
// company served at http://<slug>.localhost:3000. Only ever touches local
// state under .wrangler/; it never passes --remote.
//
//   npm run dev:seed -- [slug] [workos-organization-id]
import { execFileSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";

const dashboard = fileURLToPath(new URL("..", import.meta.url));
const DATABASE = "pulsermm-production";
// Without a WorkOS organization the dashboard loads but sign-in cannot finish.
const PLACEHOLDER_ORGANIZATION = "org_LOCALPLACEHOLDER";

export function fixtureSql(slug = "acme", organizationId = PLACEHOLDER_ORGANIZATION, now = Date.now()) {
  if (!/^(?=.{2,63}$)[a-z0-9](?:[a-z0-9-]*[a-z0-9])$/.test(slug)) throw new Error(`invalid company slug ${JSON.stringify(slug)}`);
  if (!/^org_[A-Za-z0-9]{1,64}$/.test(organizationId)) throw new Error(`invalid WorkOS organization ID ${JSON.stringify(organizationId)}`);
  if (!Number.isSafeInteger(now)) throw new Error("invalid timestamp");
  const id = `local-${slug}`;
  return [
    `INSERT INTO companies (id, name, created_at, slug, status, workos_organization_id, updated_at)`,
    `VALUES ('${id}', '${slug} (local)', ${now}, '${slug}', 'active', '${organizationId}', ${now})`,
    `ON CONFLICT(id) DO UPDATE SET status = 'active', workos_organization_id = excluded.workos_organization_id, updated_at = excluded.updated_at;`,
    `INSERT OR IGNORE INTO company_domains (hostname, company_id, kind, created_at)`,
    `VALUES ('${slug}.localhost', '${id}', 'primary', ${now});`,
  ].join("\n");
}

function wrangler(...args) {
  execFileSync("npx", ["wrangler", ...args, "--local", "--persist-to", ".wrangler/state", "--config", "../server/wrangler.jsonc"], {
    cwd: dashboard,
    stdio: "inherit",
    env: { ...process.env, CI: "1" },
  });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [slug = "acme", organizationId = PLACEHOLDER_ORGANIZATION] = process.argv.slice(2);
  const sql = fixtureSql(slug, organizationId);
  wrangler("d1", "migrations", "apply", DATABASE);
  wrangler("d1", "execute", DATABASE, "--command", sql);
  console.log(`\nSeeded http://${slug}.localhost:3000 for WorkOS organization ${organizationId}.`);
  if (organizationId === PLACEHOLDER_ORGANIZATION) {
    console.log("Pass a WorkOS staging organization ID as the second argument to sign in.");
  }
}
