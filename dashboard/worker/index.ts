import handler from "vinext/server/fetch-handler";

const ROOT_DOMAIN = "meshrmm.com";
const RESERVED_HOSTS = new Set(["admin", "api", "auth", "downloads", "status", "support", "www"]);

function tenantSlug(hostname: string) {
  const suffix = `.${ROOT_DOMAIN}`;
  if (!hostname.endsWith(suffix)) return null;
  const slug = hostname.slice(0, -suffix.length);
  if (
    !/^(?=.{2,63}$)[a-z0-9](?:[a-z0-9-]*[a-z0-9])$/.test(slug) ||
    RESERVED_HOSTS.has(slug)
  ) return null;
  return slug;
}

function notFound() {
  return new Response("Company not found", {
    status: 404,
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-store",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

const worker = {
  async fetch(request, env, ctx): Promise<Response> {
    const url = new URL(request.url);
    let appRequest: Request = request;
    if (url.hostname === "www.meshrmm.com") {
      url.hostname = "meshrmm.com";
      return Response.redirect(url, 308);
    }

    if (url.hostname === `auth.${ROOT_DOMAIN}`) {
      if (url.pathname !== "/login") return notFound();
      url.pathname = "/v1/auth/invitations/resolve";
      return env.MESHRMM_API.fetch(new Request(url, { headers: request.headers }));
    }

    if (url.pathname.startsWith("/v1/") && url.hostname === ROOT_DOMAIN) return notFound();
    if (url.pathname === "/healthz" || url.pathname.startsWith("/v1/")) {
      return env.MESHRMM_API.fetch(request);
    }

    if (url.hostname !== ROOT_DOMAIN && url.hostname !== `admin.${ROOT_DOMAIN}`) {
      const slug = tenantSlug(url.hostname);
      if (!slug) return notFound();
      const company = await env.DB.prepare(
        "SELECT workos_organization_id FROM companies WHERE slug = ?1 COLLATE NOCASE AND status IN ('active', 'awaiting_admin')",
      ).bind(slug).first<{ workos_organization_id: string | null }>();
      if (!company) return notFound();
      if (!company.workos_organization_id) {
        return new Response("Company provisioning is not complete", {
          status: 503,
          headers: { "Cache-Control": "no-store", "Retry-After": "30" },
        });
      }
      const headers = new Headers(request.headers);
      headers.set("X-Mesh-Tenant-Slug", slug);
      headers.set("X-Mesh-WorkOS-Organization-Id", company.workos_organization_id);
      appRequest = new Request(request, { headers });
    }

    return handler.fetch(appRequest, env, ctx);
  },
} satisfies ExportedHandler<Env>;

export default worker;
