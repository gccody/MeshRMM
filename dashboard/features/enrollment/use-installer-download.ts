"use client";

import { type FormEvent, useCallback, useState } from "react";
import { type AuthorizedFetch, errorMessage } from "../../lib/http";
import {
  type AgentInstallerBootstrap,
  type AgentPlatform,
  INSTALLER_ASSETS,
  enrolledInstaller,
  publishedChecksum,
  sha256Hex,
} from "./installer";

// Authorizes a one-time enrollment, verifies the published installer and
// downloads it with the enrollment appended.
export function useInstallerDownload(authorizedFetch: AuthorizedFetch) {
  const [platform, setPlatform] = useState<AgentPlatform>("windows-x64");
  const [isDownloading, setIsDownloading] = useState(false);
  const [downloaded, setDownloaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Starts a fresh "Add device" flow.
  const reset = useCallback(() => {
    setPlatform("windows-x64");
    setDownloaded(false);
    setError(null);
  }, []);

  const download = async (event: FormEvent) => {
    event.preventDefault();
    setIsDownloading(true);
    setError(null);
    try {
      const bootstrapResponse = await authorizedFetch("/v1/agent-installers", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ platform }),
      });
      if (!bootstrapResponse.ok) {
        throw new Error(await errorMessage(bootstrapResponse, "The Agent installer could not be authorized."));
      }
      const bootstrap = (await bootstrapResponse.json()) as AgentInstallerBootstrap;
      const asset = INSTALLER_ASSETS[platform];
      const [binaryResponse, checksumResponse] = await Promise.all([
        fetch(asset.binary, { cache: "no-store" }),
        fetch(asset.checksum, { cache: "no-store" }),
      ]);
      if (!binaryResponse.ok || !checksumResponse.ok) {
        throw new Error("The selected Agent installer has not been published yet.");
      }
      const binary = await binaryResponse.arrayBuffer();
      const expectedChecksum = publishedChecksum(await checksumResponse.text());
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
      setDownloaded(true);
    } catch (downloadError) {
      setError(downloadError instanceof Error ? downloadError.message : "The Agent installer could not be created.");
    } finally {
      setIsDownloading(false);
    }
  };

  return { platform, setPlatform, isDownloading, downloaded, error, reset, download };
}
