import { existsSync } from "node:fs";
import { cloudflare } from "@cloudflare/vite-plugin";
import vinext from "vinext";
import { defineConfig } from "vite";

// macOS Seatbelt blocks FSEvents, so Codex previews need polling for HMR.
const isCodexSeatbeltSandbox = process.env.CODEX_SANDBOX === "seatbelt";

// `npm run dev` also runs the control-plane Worker, sharing local D1, once it
// has been built with `worker-build` (see README). It is never deployed here.
const serverBuilt = existsSync(new URL("../server/build/index.js", import.meta.url));

export default defineConfig(async ({ command }) => {
  return {
    server: isCodexSeatbeltSandbox
      ? { watch: { useFsEvents: false, usePolling: true } }
      : undefined,
    plugins: [
      vinext(),
      cloudflare({
        viteEnvironment: { name: "rsc", childEnvironments: ["ssr"] },
        auxiliaryWorkers: command === "serve" && serverBuilt
          ? [{ configPath: "../server/wrangler.jsonc", devOnly: true }]
          : [],
      }),
    ],
  };
});
