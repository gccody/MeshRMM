import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import { headers } from "next/headers";
import "./globals.css";
import Providers from "./providers";

const geistSans = Geist({ variable: "--font-geist-sans", subsets: ["latin"] });
const geistMono = Geist_Mono({ variable: "--font-geist-mono", subsets: ["latin"] });

export async function generateMetadata(): Promise<Metadata> {
  const incoming = await headers();
  const host = incoming.get("host") ?? incoming.get("x-forwarded-host") ?? "localhost:3000";
  const hostname = host.split(":")[0].toLowerCase();
  const protocol = incoming.get("x-forwarded-proto") ?? (host.startsWith("localhost") ? "http" : "https");
  const image = `${protocol}://${host}/og.png`;
  const title = hostname === "meshrmm.com"
    ? "MeshRMM | Secure remote monitoring"
    : hostname === "admin.meshrmm.com"
      ? "Platform Admin | MeshRMM"
      : "Agents | MeshRMM";
  const description = hostname === "meshrmm.com"
    ? "Company-isolated remote monitoring and management with secure endpoint access."
    : "Monitor connected agents and launch secure remote desktop sessions from MeshRMM.";
  return {
    title,
    description,
    openGraph: { title, description, type: "website", images: [{ url: image, width: 1200, height: 630, alt: "MeshRMM agents dashboard" }] },
    twitter: { card: "summary_large_image", title, description, images: [image] },
  };
}

export default async function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  const incoming = await headers();
  const rawHost = incoming.get("host") ?? incoming.get("x-forwarded-host") ?? "localhost:3000";
  const hostname = rawHost.split(":")[0].toLowerCase();
  const protocol = incoming.get("x-forwarded-proto") ?? (hostname === "localhost" ? "http" : "https");
  const origin = `${protocol}://${rawHost}`;
  const surface = hostname === "meshrmm.com"
    ? "marketing"
    : hostname === "admin.meshrmm.com"
      ? "platform"
      : "tenant";
  const clientId = process.env.WORKOS_CLIENT_ID ?? "client_01M0FT1AN01PAT37N98EMRSNVW";
  const redirectUri = surface === "marketing" ? (process.env.WORKOS_REDIRECT_URI ?? origin) : origin;
  const serverUrl = process.env.MESHRMM_SERVER_URL || origin;
  return (
    <html lang="en">
      <body className={`${geistSans.variable} ${geistMono.variable}`}>
        <Providers
          clientId={clientId}
          redirectUri={redirectUri}
          serverUrl={serverUrl}
          surface={surface}
          hostname={hostname}
          tenantSlug={incoming.get("x-mesh-tenant-slug") ?? undefined}
          workosOrganizationId={incoming.get("x-mesh-workos-organization-id") ?? undefined}
        >{children}</Providers>
      </body>
    </html>
  );
}
