// Which part of MeshRMM a hostname serves. Production serves meshrmm.com,
// admin.meshrmm.com and <slug>.meshrmm.com. For local development only,
// MESHRMM_DEV_ROOT_DOMAIN=localhost (set in .dev.vars, never in wrangler.jsonc)
// maps localhost, admin.localhost and <slug>.localhost the same way. Browsers
// resolve every *.localhost name to this computer, so the mapping can never
// match a request that reached a deployed Worker.
export const ROOT_DOMAIN = "meshrmm.com";
const DEV_ROOT_DOMAIN = "localhost";
const RESERVED_HOSTS = new Set(["admin", "api", "auth", "downloads", "status", "support", "www"]);

export type Host =
  | { surface: "marketing" | "platform" | "auth" | "www"; rootDomain: string }
  | { surface: "tenant"; rootDomain: string; slug: string }
  | { surface: "unknown" };

export function rootDomainFor(hostname: string, devRootDomain?: string) {
  if (
    devRootDomain === DEV_ROOT_DOMAIN &&
    (hostname === DEV_ROOT_DOMAIN || hostname.endsWith(`.${DEV_ROOT_DOMAIN}`))
  ) return DEV_ROOT_DOMAIN;
  return ROOT_DOMAIN;
}

export function classifyHost(hostname: string, devRootDomain?: string): Host {
  const rootDomain = rootDomainFor(hostname, devRootDomain);
  if (hostname === rootDomain) return { surface: "marketing", rootDomain };
  if (hostname === `admin.${rootDomain}`) return { surface: "platform", rootDomain };
  if (hostname === `auth.${rootDomain}`) return { surface: "auth", rootDomain };
  if (hostname === `www.${rootDomain}`) return { surface: "www", rootDomain };
  const suffix = `.${rootDomain}`;
  if (!hostname.endsWith(suffix)) return { surface: "unknown" };
  const slug = hostname.slice(0, -suffix.length);
  if (
    !/^(?=.{2,63}$)[a-z0-9](?:[a-z0-9-]*[a-z0-9])$/.test(slug) ||
    RESERVED_HOSTS.has(slug)
  ) return { surface: "unknown" };
  return { surface: "tenant", rootDomain, slug };
}
