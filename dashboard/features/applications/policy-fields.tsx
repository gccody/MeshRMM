import { LoaderCircle } from "lucide-react";
import { useCallback } from "react";
import { useResource } from "../../lib/use-resource";
import { PLATFORM_LABELS } from "../agents/types";
import type { PolicyDraft } from "../automation/model";
import { useWorkspace } from "../workspace/workspace-context";
import { fetchLibrary } from "./applications-api";
import { MODE_LABELS, type UpdateMode, platformsOf } from "./model";

const MODES: readonly UpdateMode[] = ["all", "only", "except"];

// What an application policy installs, or which outdated applications it
// updates, for the policy editor.
export function ApplicationPolicyFields({ install, applicationIds, packages, mode, onChange }: {
  install: boolean;
  applicationIds: readonly string[];
  packages: string;
  mode: UpdateMode;
  onChange: (change: Partial<Pick<PolicyDraft, "applicationIds" | "packages" | "updateMode">>) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const load = useCallback(() => fetchLibrary(authorizedFetch), [authorizedFetch]);
  const { data, error } = useResource(load, "The application library could not be loaded.");
  const missing = data ? applicationIds.filter((id) => !data.some((application) => application.id === id)) : [];
  const toggle = (id: string, checked: boolean) =>
    onChange({ applicationIds: checked ? [...applicationIds, id] : applicationIds.filter((candidate) => candidate !== id) });
  const choosing = install || mode !== "all";

  return (
    <>
      {!install && (
        <fieldset className="choice-list">
          <legend>Update</legend>
          {MODES.map((choice) => (
            <label key={choice}>
              <input type="radio" name="policy-update-mode" checked={mode === choice} onChange={() => onChange({ updateMode: choice })} />
              <span><strong>{MODE_LABELS[choice]}</strong>{choice === "all" ? "Whatever each device’s package managers list as outdated." : "Choose them below: library applications, or other packages by ID."}</span>
            </label>
          ))}
        </fieldset>
      )}
      {choosing && (
        <fieldset className="choice-list picker-list">
          <legend>{install ? "Install" : mode === "only" ? "Applications to update" : "Applications to leave out"}</legend>
          {!data && !error && <p className="field-help"><LoaderCircle size={14} className="spin" aria-hidden="true" /> Loading the library…</p>}
          {error && <p className="installer-error" role="alert">{error}</p>}
          {data && !data.length && <p className="field-help">The library has no applications yet. Add them on the Applications page.</p>}
          {missing.map((id) => (
            <label key={id}>
              <input type="checkbox" checked onChange={() => toggle(id, false)} />
              <span><strong>Deleted application</strong>No longer in the library</span>
            </label>
          ))}
          {data?.map((application) => (
            <label key={application.id}>
              <input type="checkbox" checked={applicationIds.includes(application.id)} onChange={(event) => toggle(application.id, event.target.checked)} />
              <span><strong>{application.name}</strong>{platformsOf(application).map((platform) => PLATFORM_LABELS[platform]).join(", ")}</span>
            </label>
          ))}
        </fieldset>
      )}
      {!install && mode !== "all" && (
        <label htmlFor="policy-packages">Other packages
          <textarea id="policy-packages" rows={3} value={packages} onChange={(event) => onChange({ packages: event.target.value })} placeholder={"winget:Mozilla.Firefox\nhomebrew_cask:google-chrome"} spellCheck={false} />
          <small className="field-help">One a line, as source:id. The sources are winget, chocolatey, homebrew, homebrew_cask, flatpak and snap.</small>
        </label>
      )}
      {install && <p className="field-help">Devices that can’t use any of the applications’ sources, on another operating system or without the package manager, are skipped with the reason, not failed, and raise no alert. With winget and Chocolatey, applications the library pins install at that version.</p>}
      {!install && <p className="field-help">Applications the library pins to a version are never updated.</p>}
    </>
  );
}
