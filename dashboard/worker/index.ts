import handler from "vinext/server/fetch-handler";
import { handleAuth } from "./auth";
import { classifyHost, type Host } from "../lib/hosts";
import { recordD1, startUsage, writeUsage, type Usage } from "./usage";

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

async function route(request: Request, env: Env, ctx: ExecutionContext, host: Host, usage: Usage): Promise<Response> {
  const url = new URL(request.url);
  let appRequest: Request = request;
  let organizationId: string | undefined;
  if (host.surface === "www") {
    url.hostname = host.rootDomain;
    return Response.redirect(url, 308);
  }

  if (host.surface === "auth") {
    if (url.pathname !== "/login") return notFound();
    url.pathname = "/v1/auth/invitations/resolve";
    // The company redirect belongs in the browser. Following it through the
    // service binding sends /login back to the API, which has no such route.
    return env.MESHRMM_API.fetch(new Request(url, { headers: request.headers, redirect: "manual" }));
  }

  if (url.pathname.startsWith("/v1/") && host.surface === "marketing") return notFound();
  if (url.pathname === "/healthz" || url.pathname.startsWith("/v1/")) {
    return env.MESHRMM_API.fetch(request);
  }

  if (host.surface !== "marketing" && host.surface !== "platform") {
    if (host.surface !== "tenant") return notFound();
    const { slug } = host;
    // run() rather than first(): only full results report the rows D1 bills.
    const lookup = await env.DB.prepare(
      "SELECT id, workos_organization_id FROM companies WHERE slug = ?1 COLLATE NOCASE AND status IN ('active', 'awaiting_admin')",
    ).bind(slug).run<{ id: string; workos_organization_id: string | null }>();
    recordD1(usage, lookup.meta);
    const company = lookup.results[0];
    if (!company) return notFound();
    usage.owner = company.id;
    if (!company.workos_organization_id) {
      return new Response("Company provisioning is not complete", {
        status: 503,
        headers: { "Cache-Control": "no-store", "Retry-After": "30" },
      });
    }
    organizationId = company.workos_organization_id;
    const headers = new Headers(request.headers);
    headers.set("X-Mesh-Tenant-Slug", slug);
    headers.set("X-Mesh-WorkOS-Organization-Id", company.workos_organization_id);
    appRequest = new Request(request, { headers });
  }

  if (url.pathname.startsWith("/auth/")) {
    if (host.surface === "marketing") return notFound();
    return handleAuth(appRequest, env, organizationId);
  }

  return handler.fetch(appRequest, env, ctx);
}

const worker = {
  async fetch(request, env, ctx): Promise<Response> {
    const host = classifyHost(new URL(request.url).hostname, env.MESHRMM_DEV_ROOT_DOMAIN);
    const usage = startUsage(host);
    try {
      return await route(request, env, ctx, host, usage);
    } finally {
      writeUsage(env.USAGE, usage);
    }
  },
} satisfies ExportedHandler<Env>;

export default worker;
