// The native viewer builds each server release ships (see
// scripts/release-artifacts.mjs). A server without them, as in local
// development, answers these links with 404s.
export type ViewerPlatform = "windows-x64" | "macos-arm64" | "linux-x64" | "linux-arm64";

export const VIEWER_PLATFORMS: readonly ViewerPlatform[] = ["windows-x64", "macos-arm64", "linux-x64", "linux-arm64"];

// A browser saves the Linux build without the right to run it.
const LINUX_SETUP = "Allow the downloaded file to run (chmod +x, or Properties → Executable in your file manager) and open it once to install it. You can then delete the download.";

export const VIEWER_DOWNLOADS: Record<ViewerPlatform, { os: string; label: string; href: string; setup: string }> = {
  "windows-x64": {
    os: "Windows",
    label: "Windows 10/11 (x64)",
    href: "/downloads/meshrmm-remote-windows-x64.exe",
    setup: "Open the downloaded MeshRMM Remote once to install it. You can then delete the download.",
  },
  "macos-arm64": {
    // Safari cannot report the CPU, so the name says which build this is.
    os: "macOS (Apple silicon)",
    label: "macOS (Apple silicon)",
    href: "/downloads/meshrmm-remote-macos-arm64.zip",
    setup: "Unzip it and open MeshRMM Remote once to install it in your Applications folder.",
  },
  "linux-x64": {
    os: "Linux (x64)",
    label: "Linux (x64)",
    href: "/downloads/meshrmm-remote-linux-x64",
    setup: LINUX_SETUP,
  },
  "linux-arm64": {
    os: "Linux (arm64)",
    label: "Linux (arm64)",
    href: "/downloads/meshrmm-remote-linux-arm64",
    setup: LINUX_SETUP,
  },
};

export type NavigatorLike = {
  userAgent?: string;
  maxTouchPoints?: number;
  userAgentData?: { platform?: string } | null;
};

// The viewer build for this browser's computer, or null when there is none or
// the platform is unclear (the caller then offers every build).
export function detectViewerPlatform(nav: NavigatorLike | null | undefined): ViewerPlatform | null {
  if (!nav) return null;
  const userAgent = nav.userAgent ?? "";
  // Windows on ARM runs the x64 build under emulation.
  if (nav.userAgentData?.platform === "Windows" || /Windows NT/.test(userAgent)) return "windows-x64";
  // iPadOS asks for desktop sites with a Mac user agent; only a touch screen
  // tells them apart.
  if (/Macintosh/.test(userAgent) && (nav.maxTouchPoints ?? 0) <= 1) return "macos-arm64";
  // Android and ChromeOS say Linux too, and run neither build.
  const platform = nav.userAgentData?.platform;
  if (platform === "Android" || platform === "Chrome OS" || /Android|CrOS/.test(userAgent)) return null;
  if (platform === "Linux" || /Linux/.test(userAgent)) {
    // Chrome says x86_64 on every Linux computer, so an ARM one finds its
    // build under the other platforms.
    return /aarch64|arm64/.test(userAgent) ? "linux-arm64" : "linux-x64";
  }
  return null;
}
