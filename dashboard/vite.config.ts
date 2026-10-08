import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `npm run dev` serves the website and sends API, socket and download requests
// to a MeshRMM server running in proxy mode. The server only accepts changes
// and event sockets from its own public origin, so the proxy presents that
// origin: set MESHRMM_PUBLIC_URL to the server's public_url.
const server = process.env.MESHRMM_SERVER ?? "http://127.0.0.1:8080";
const publicOrigin = new URL(process.env.MESHRMM_PUBLIC_URL ?? "https://localhost").origin;

const toServer = {
  target: server,
  ws: true,
  configure(proxy: { on(event: string, listener: (request: { setHeader(name: string, value: string): void }) => void): void }) {
    const presentOrigin = (request: { setHeader(name: string, value: string): void }) => request.setHeader("origin", publicOrigin);
    proxy.on("proxyReq", presentOrigin);
    proxy.on("proxyReqWs", presentOrigin);
  },
};

// macOS Seatbelt blocks FSEvents, so Codex previews need polling for HMR.
const isCodexSeatbeltSandbox = process.env.CODEX_SANDBOX === "seatbelt";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/v1": toServer,
      "/downloads": toServer,
      "/healthz": toServer,
    },
    watch: isCodexSeatbeltSandbox ? { useFsEvents: false, usePolling: true } : undefined,
  },
  build: {
    // The server embeds the build; a source map would only add size.
    sourcemap: false,
  },
});
