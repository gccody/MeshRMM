import { headers } from "next/headers";
import { classifyHost } from "./hosts";

export type Surface = "marketing" | "platform" | "tenant";

// The local dev server serves localhost and *.localhost over plain HTTP.
function defaultProtocol(hostname: string) {
  return hostname === "localhost" || hostname.endsWith(".localhost") ? "http" : "https";
}

// Describes the host this server render is for. The Worker rejects unknown
// tenant hostnames before rendering, so every other surface renders the tenant
// workspace.
export async function requestHost() {
  const incoming = await headers();
  const rawHost = incoming.get("host") ?? incoming.get("x-forwarded-host") ?? "localhost:3000";
  const hostname = rawHost.split(":")[0].toLowerCase();
  const protocol = incoming.get("x-forwarded-proto") ?? defaultProtocol(hostname);
  const host = classifyHost(hostname, process.env.MESHRMM_DEV_ROOT_DOMAIN);
  const surface: Surface = host.surface === "marketing" || host.surface === "platform" ? host.surface : "tenant";
  return { headers: incoming, rawHost, hostname, protocol, origin: `${protocol}://${rawHost}`, surface };
}

export async function requestSurface(): Promise<Surface> {
  return (await requestHost()).surface;
}
