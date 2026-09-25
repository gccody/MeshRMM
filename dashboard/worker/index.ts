import handler from "vinext/server/fetch-handler";
import { handleAuth } from "./auth";
import { classifyHost } from "../lib/hosts";

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
    let organizationId: string | undefined;
    const host = classifyHost(url.hostname, env.MESHRMM_DEV_ROOT_DOMAIN);
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
  },
} satisfies ExportedHandler<Env>;

export default worker;
