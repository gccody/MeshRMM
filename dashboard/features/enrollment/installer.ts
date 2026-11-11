// Builds an enrolled Agent installer: for Windows, the published binary
// followed by the bootstrap JSON and a trailer the Agent reads from the end of
// its own file; for macOS and Linux, a terminal command, since appending to a
// signed Mac app would break its signature.
export type AgentPlatform = WindowsPlatform | "macos" | "linux";
// Windows installs from a downloaded installer, one for each architecture.
export type WindowsPlatform = "windows-x64" | "windows-arm64";

export function isWindowsPlatform(platform: AgentPlatform): platform is WindowsPlatform {
  return platform === "windows-x64" || platform === "windows-arm64";
}

export type AgentInstallerBootstrap = {
  server: string;
  install_token: string;
  expires_at_unix_ms: number;
};

// The server's update manifest lists each build it ships with its SHA-256.
export const UPDATE_MANIFEST = "/downloads/update-manifest.json";

export const INSTALLER_ASSETS: Record<WindowsPlatform, { label: string; binary: string; target: string; fileName: string }> = {
  "windows-x64": {
    label: "Windows 10/11 (x64)",
    binary: "/downloads/meshrmm-agent-windows-x64.exe",
    target: "agent-windows-x64",
    fileName: "MeshRMM-Agent-Setup-Windows-x64.exe",
  },
  "windows-arm64": {
    label: "Windows 11 (ARM64)",
    binary: "/downloads/meshrmm-agent-windows-arm64.exe",
    target: "agent-windows-arm64",
    fileName: "MeshRMM-Agent-Setup-Windows-ARM64.exe",
  },
};

export const ENROLLMENT_MAGIC = "MESHRMM-BOOTSTRAP-V1";

// The commands that install the Mac and Linux Agents from the server at
// `origin`. The Agent reads the authorization as hexadecimal JSON, which needs
// no quoting. Without `sudo`, for what already runs as root, such as Jamf's
// scripts.
export function macInstallCommand(origin: string, bootstrap: AgentInstallerBootstrap, { sudo = true } = {}): string {
  const { base, authorization } = installArguments(origin, bootstrap);
  return `curl -fsSL ${base}/install-agent-macos.sh | ${sudo ? "sudo " : ""}/bin/sh -s -- ${base} ${authorization}`;
}

export function linuxInstallCommand(origin: string, bootstrap: AgentInstallerBootstrap, { sudo = true } = {}): string {
  const { base, authorization } = installArguments(origin, bootstrap);
  return `curl -fsSL ${base}/install-agent-linux.sh | ${sudo ? "sudo " : ""}sh -s -- ${base} ${authorization}`;
}

// The command line that installs a downloaded Windows installer unattended,
// as Group Policy, Intune or a script runs it: already elevated, with no
// window, reporting through its exit code, and leaving an installed Agent
// alone.
export function windowsQuietCommand(platform: WindowsPlatform): string {
  return `${INSTALLER_ASSETS[platform].fileName} --quiet`;
}

function installArguments(origin: string, bootstrap: AgentInstallerBootstrap) {
  const authorization = Array.from(new TextEncoder().encode(JSON.stringify(bootstrap)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  return { base: origin.replace(/\/+$/, ""), authorization };
}

// The SHA-256 the update manifest gives `target`'s build, or null when it
// lists none.
export function publishedChecksum(manifest: unknown, target: string): string | null {
  const releases = (manifest as { releases?: Record<string, { sha256?: unknown }> } | null)?.releases;
  const checksum = releases?.[target]?.sha256;
  return typeof checksum === "string" && /^[a-fA-F0-9]{64}$/.test(checksum) ? checksum.toLowerCase() : null;
}

export async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

// binary || config JSON || u64 little-endian config length || magic
export function enrolledInstaller(binary: ArrayBuffer, bootstrap: AgentInstallerBootstrap): Blob {
  const config = new TextEncoder().encode(JSON.stringify(bootstrap));
  const magic = new TextEncoder().encode(ENROLLMENT_MAGIC);
  const trailer = new Uint8Array(8 + magic.length);
  new DataView(trailer.buffer).setBigUint64(0, BigInt(config.length), true);
  trailer.set(magic, 8);
  return new Blob([binary, config, trailer], { type: "application/vnd.microsoft.portable-executable" });
}

// Fetches the published Windows installer for `platform`, checks it against
// the update manifest's SHA-256, and saves it with `bootstrap` appended.
export async function downloadEnrolledInstaller(platform: WindowsPlatform, bootstrap: AgentInstallerBootstrap) {
  const asset = INSTALLER_ASSETS[platform];
  const [binaryResponse, manifestResponse] = await Promise.all([
    fetch(asset.binary, { cache: "no-store" }),
    fetch(UPDATE_MANIFEST, { cache: "no-store" }),
  ]);
  if (!binaryResponse.ok || !manifestResponse.ok) {
    throw new Error("The selected Agent installer has not been published yet.");
  }
  const binary = await binaryResponse.arrayBuffer();
  const expectedChecksum = publishedChecksum(await manifestResponse.json(), asset.target);
  if (!expectedChecksum) {
    throw new Error("The published Agent installer checksum is invalid.");
  }
  if ((await sha256Hex(binary)) !== expectedChecksum) {
    throw new Error("The Agent installer failed its SHA-256 integrity check.");
  }
  const downloadUrl = URL.createObjectURL(enrolledInstaller(binary, bootstrap));
  const link = document.createElement("a");
  link.href = downloadUrl;
  link.download = asset.fileName;
  document.body.appendChild(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(downloadUrl), 1_000);
}
