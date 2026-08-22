import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import { headers } from "next/headers";
import "./globals.css";
import Providers from "./providers";

const geistSans = Geist({ variable: "--font-geist-sans", subsets: ["latin"] });
const geistMono = Geist_Mono({ variable: "--font-geist-mono", subsets: ["latin"] });

const title = "Agents | MeshRMM";
const description = "Monitor connected agents and launch secure remote desktop sessions from MeshRMM.";

export async function generateMetadata(): Promise<Metadata> {
  const incoming = await headers();
  const host = incoming.get("x-forwarded-host") ?? incoming.get("host") ?? "localhost:3000";
  const protocol = incoming.get("x-forwarded-proto") ?? (host.startsWith("localhost") ? "http" : "https");
  const image = `${protocol}://${host}/og.png`;
  return {
    title,
    description,
    openGraph: { title, description, type: "website", images: [{ url: image, width: 1200, height: 630, alt: "MeshRMM agents dashboard" }] },
    twitter: { card: "summary_large_image", title, description, images: [image] },
  };
}

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  const clientId = process.env.WORKOS_CLIENT_ID ?? "client_01M0FT1AN01PAT37N98EMRSNVW";
  const redirectUri = process.env.WORKOS_REDIRECT_URI ?? "https://meshrmm.com";
  const serverUrl = process.env.MESHRMM_SERVER_URL ?? "https://meshrmm-server.gccody2010.workers.dev";
  return (
    <html lang="en">
      <body className={`${geistSans.variable} ${geistMono.variable}`}>
        <Providers clientId={clientId} redirectUri={redirectUri} serverUrl={serverUrl}>{children}</Providers>
      </body>
    </html>
  );
}
