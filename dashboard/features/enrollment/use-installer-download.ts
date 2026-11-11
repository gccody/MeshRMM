
import { type FormEvent, useCallback, useState } from "react";
import { type AuthorizedFetch, errorMessage } from "../../lib/http";
import {
  type AgentInstallerBootstrap,
  type AgentPlatform,
  downloadEnrolledInstaller,
  isWindowsPlatform,
  linuxInstallCommand,
  macInstallCommand,
} from "./installer";

// Authorizes a one-time enrollment. For Windows it verifies the published
// installer and downloads it with the enrollment appended; for macOS and Linux
// it shows the terminal command that installs the Agent.
export function useInstallerDownload(authorizedFetch: AuthorizedFetch) {
  const [platform, setPlatform] = useState<AgentPlatform>("windows-x64");
  const [isDownloading, setIsDownloading] = useState(false);
  const [downloaded, setDownloaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [command, setCommand] = useState<string | null>(null);
  // The company the device joins; "" for none.
  const [companyId, setCompanyIdState] = useState("");

  // Starts a fresh "Add device" flow, enrolling into `company` if given.
  const reset = useCallback((company = "") => {
    setPlatform("windows-x64");
    setCompanyIdState(company);
    setDownloaded(false);
    setError(null);
    setCommand(null);
  }, []);

  const changePlatform = useCallback((next: AgentPlatform) => {
    setPlatform(next);
    setDownloaded(false);
    setError(null);
    setCommand(null);
  }, []);

  // A command made for another company would enroll into that one.
  const setCompanyId = useCallback((next: string) => {
    setCompanyIdState(next);
    setDownloaded(false);
    setError(null);
    setCommand(null);
  }, []);

  const download = async (event: FormEvent) => {
    event.preventDefault();
    setIsDownloading(true);
    setError(null);
    try {
      const bootstrapResponse = await authorizedFetch("/v1/agent-installers", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ platform, company_id: companyId || null }),
      });
      if (!bootstrapResponse.ok) {
        throw new Error(await errorMessage(bootstrapResponse, "The Agent installer could not be authorized."));
      }
      const bootstrap = (await bootstrapResponse.json()) as AgentInstallerBootstrap;
      if (!isWindowsPlatform(platform)) {
        setCommand((platform === "macos" ? macInstallCommand : linuxInstallCommand)(bootstrap.server, bootstrap));
        setDownloaded(true);
        return;
      }
      await downloadEnrolledInstaller(platform, bootstrap);
      setDownloaded(true);
    } catch (downloadError) {
      setError(downloadError instanceof Error ? downloadError.message : "The Agent installer could not be created.");
    } finally {
      setIsDownloading(false);
    }
  };

  return { platform, setPlatform: changePlatform, companyId, setCompanyId, isDownloading, downloaded, error, command, reset, download };
}
