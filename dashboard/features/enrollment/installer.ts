// Builds an enrolled Agent installer: for Windows, the published binary
// followed by the bootstrap JSON and a trailer the Agent reads from the end of
// its own file; for macOS, a Terminal command, since appending to a signed Mac
// app would break its signature.
export type AgentPlatform = "windows-x64" | "macos";

export type AgentInstallerBootstrap = {
  server: string;
  install_token: string;
  expires_at_unix_ms: number;
};

export const INSTALLER_ASSETS: Record<"windows-x64", { label: string; binary: string; checksum: string; fileName: string }> = {
  "windows-x64": {
    label: "Windows 10/11 (x64)",
    binary: "/downloads/meshrmm-agent-windows-x64.exe",
    checksum: "/downloads/meshrmm-agent-windows-x64.exe.sha256",
    fileName: "MeshRMM-Agent-Setup-Windows-x64.exe",
  },
};

export const ENROLLMENT_MAGIC = "MESHRMM-BOOTSTRAP-V1";

// The command that installs the Mac Agent from this dashboard's origin. The
// Agent reads the authorization as hexadecimal JSON, which needs no quoting.
export function macInstallCommand(origin: string, bootstrap: AgentInstallerBootstrap): string {
  const authorization = Array.from(new TextEncoder().encode(JSON.stringify(bootstrap)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  const base = origin.replace(/\/+$/, "");
  return `curl -fsSL ${base}/install-agent-macos.sh | sudo /bin/sh -s -- ${base} ${authorization}`;
}

// The first field of a `sha256sum`-style file, or null when it is not a
// SHA-256 digest.
export function publishedChecksum(text: string): string | null {
  const checksum = text.trim().split(/\s+/)[0]?.toLowerCase();
  return checksum?.match(/^[a-f0-9]{64}$/) ? checksum : null;
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
