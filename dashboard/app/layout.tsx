import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import "./globals.css";
import Providers from "./providers";
import { requestHost } from "../lib/request-surface";

const geistSans = Geist({ variable: "--font-geist-sans", subsets: ["latin"] });
const geistMono = Geist_Mono({ variable: "--font-geist-mono", subsets: ["latin"] });

export async function generateMetadata(): Promise<Metadata> {
  const { origin, surface } = await requestHost();
  const image = `${origin}/og.png`;
  const title = surface === "marketing"
    ? "MeshRMM | Secure remote monitoring"
    : surface === "platform"
      ? "Platform Admin | MeshRMM"
      : "Devices | MeshRMM";
  const description = surface === "marketing"
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
  const { headers: incoming, hostname, origin, surface } = await requestHost();
  const serverUrl = process.env.MESHRMM_SERVER_URL || origin;
  return (
    <html lang="en">
      <body className={`${geistSans.variable} ${geistMono.variable}`}>
        <Providers
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
