import { LoaderCircle, Plus, Trash2, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { type DevicePlatform, PLATFORM_LABELS } from "../agents/types";
import { type ToolboxFile, formatBytes, groupByFolder } from "../toolbox/model";
import { useWorkspace } from "../workspace/workspace-context";
import { saveApplication } from "./applications-api";
import {
  type Application,
  type ApplicationDraft,
  type DetectionKind,
  INSTALLER_KINDS,
  MAX_SOURCES,
  type PackageSource,
  SOURCES,
  SOURCE_EXAMPLES,
  SOURCE_LABELS,
  SOURCE_PLATFORM,
  type SourceDraft,
  applicationDraft,
  applicationDraftProblem,
  applicationInput,
  sourceDraft,
} from "./model";

const DETECTION_LABELS: Record<DetectionKind, string> = {
  application: "Installed application",
  file: "File or folder",
  registry: "Registry key",
};

// Adds an application to the library, or changes one.
export function ApplicationEditor({ application, files, returnFocus, onClose, onSaved }: {
  application: Application | null;
  // The toolbox's shared files, for custom installers; null while loading
  // or when the user may not see them.
  files: ToolboxFile[] | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved: (application: Application) => void;
}) {
  const { authorizedFetch, can } = useWorkspace();
  const titleId = useId();
  const [draft, setDraft] = useState<ApplicationDraft>(() => applicationDraft(application));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const problem = applicationDraftProblem(draft);
  const update = (change: Partial<ApplicationDraft>) => setDraft((current) => ({ ...current, ...change }));
  const updateSource = (index: number, change: Partial<SourceDraft>) =>
    setDraft((current) => ({ ...current, sources: current.sources.map((source, at) => (at === index ? { ...source, ...change } : source)) }));
  const removeSource = (index: number) => setDraft((current) => ({ ...current, sources: current.sources.filter((_, at) => at !== index) }));
  const addSource = () => setDraft((current) => ({ ...current, sources: [...current.sources, sourceDraft({ source: "winget" })] }));
  // Custom installers run as SYSTEM or root; adding one needs more access.
  const usesCustom = draft.sources.some((source) => source.source === "custom");

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onSaved(await saveApplication(authorizedFetch, application?.id ?? null, applicationInput(draft)));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The application could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal role-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{application ? `Edit ${application.name}` : "Add an application"}</h2>
      <p>List how the application installs on each operating system. Devices install it from the first way listed for theirs that works.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy || !can("applications.manage")}>
          <label htmlFor="application-name">Name<input id="application-name" required maxLength={120} value={draft.name} onChange={(event) => update({ name: event.target.value })} placeholder="Firefox" /></label>
          <label htmlFor="application-description">Description<textarea id="application-description" rows={2} value={draft.description} onChange={(event) => update({ description: event.target.value })} placeholder="Optional" /></label>
          <label htmlFor="application-version">Pinned version
            <input id="application-version" value={draft.pinnedVersion} onChange={(event) => update({ pinnedVersion: event.target.value })} placeholder="Latest" />
          </label>
          <small className="field-help">A pinned version installs exactly that version with winget and Chocolatey, and holds the application back from updates everywhere. Leave it empty to install and keep the latest.</small>

          {draft.sources.map((source, index) => (
            <SourceFields key={index} index={index} source={source} files={files} removable={draft.sources.length > 1} onChange={(change) => updateSource(index, change)} onRemove={() => removeSource(index)} />
          ))}
          {draft.sources.length < MAX_SOURCES && <button type="button" className="secondary-button" onClick={addSource}><Plus size={15} /> Add another way to install</button>}
          {usesCustom && <p className="field-help">Custom installers run as SYSTEM on Windows and root on macOS and Linux, with the arguments and uninstall command exactly as written here.</p>}
        </fieldset>
        {(error || (problem && draft.name.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null || !can("applications.manage")}>{busy && <LoaderCircle size={16} className="spin" />} {application ? "Save application" : "Add application"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}

function SourceFields({ index, source, files, removable, onChange, onRemove }: {
  index: number;
  source: SourceDraft;
  files: ToolboxFile[] | null;
  removable: boolean;
  onChange: (change: Partial<SourceDraft>) => void;
  onRemove: () => void;
}) {
  const id = (name: string) => `application-source-${index}-${name}`;
  const custom = source.source === "custom";
  const missingFile = custom && source.fileId && files !== null && !files.some((file) => file.id === source.fileId);
  const changeSource = (next: PackageSource) =>
    onChange(next === "custom" ? { source: next } : { source: next, platform: SOURCE_PLATFORM[next] });
  return (
    <fieldset className="choice-list application-source">
      <legend>Way {index + 1}</legend>
      <div className="script-editor-row">
        <label htmlFor={id("source")}>Source
          <select id={id("source")} value={source.source} onChange={(event) => changeSource(event.target.value as PackageSource)}>
            {SOURCES.map((candidate) => <option key={candidate} value={candidate}>{SOURCE_LABELS[candidate]}{candidate === "custom" ? "" : ` (${PLATFORM_LABELS[SOURCE_PLATFORM[candidate]]})`}</option>)}
          </select>
        </label>
        {custom ? (
          <label htmlFor={id("platform")}>Operating system
            <select id={id("platform")} value={source.platform} onChange={(event) => {
              const platform = event.target.value as DevicePlatform;
              onChange(platform !== "windows" && source.detection === "registry" ? { platform, detection: "application" } : { platform });
            }}>
              {(["windows", "macos", "linux"] as const).map((platform) => <option key={platform} value={platform}>{PLATFORM_LABELS[platform]}</option>)}
            </select>
          </label>
        ) : (
          <label htmlFor={id("package")}>Package ID
            <input id={id("package")} required value={source.packageId} onChange={(event) => onChange({ packageId: event.target.value })} placeholder={SOURCE_EXAMPLES[source.source as Exclude<PackageSource, "custom">]} spellCheck={false} autoCapitalize="off" />
          </label>
        )}
      </div>
      {source.source === "flatpak" && <small className="field-help">From the system installation&apos;s remotes, such as Flathub.</small>}
      {custom && <>
        <label htmlFor={id("file")}>Installer file
          <select id={id("file")} value={source.fileId} onChange={(event) => onChange({ fileId: event.target.value })} disabled={files === null}>
            <option value="" disabled>{files === null ? "Loading the toolbox…" : files.length ? "Choose a shared file" : "No shared files in the toolbox"}</option>
            {missingFile && <option value={source.fileId} disabled>Deleted or unshared file</option>}
            {groupByFolder(files ?? []).map(({ folder, items }) => (
              <optgroup key={folder} label={folder || "Top level"}>
                {items.map((file) => <option key={file.id} value={file.id}>{file.name} ({formatBytes(file.size_bytes)})</option>)}
              </optgroup>
            ))}
          </select>
        </label>
        <small className="field-help">Upload installers to the toolbox and share them first. {PLATFORM_LABELS[source.platform]} runs {INSTALLER_KINDS[source.platform]} files.</small>
        <label htmlFor={id("arguments")}>Silent install arguments
          <input id={id("arguments")} value={source.installArguments} onChange={(event) => onChange({ installArguments: event.target.value })} placeholder={source.platform === "windows" ? "/S, or /qn /norestart for .msi files" : "Optional"} spellCheck={false} />
        </label>
        <label htmlFor={id("uninstall")}>Uninstall command
          <input id={id("uninstall")} value={source.uninstallCommand} onChange={(event) => onChange({ uninstallCommand: event.target.value })} placeholder="Optional" spellCheck={false} />
        </label>
        <small className="field-help">Runs in {source.platform === "windows" ? "cmd.exe" : "/bin/sh"} to uninstall. Without one, the library can&apos;t uninstall this way; on Windows, the device&apos;s Applications tab can still uninstall the program from its software list.</small>
        <div className="script-editor-row">
          <label htmlFor={id("detection")}>Installed when there is
            <select id={id("detection")} value={source.detection} onChange={(event) => onChange({ detection: event.target.value as DetectionKind })}>
              {(Object.keys(DETECTION_LABELS) as DetectionKind[]).filter((kind) => kind !== "registry" || source.platform === "windows").map((kind) => <option key={kind} value={kind}>{DETECTION_LABELS[kind]}</option>)}
            </select>
          </label>
          {source.detection === "application" && (
            <label htmlFor={id("detection-name")}>Named
              <input id={id("detection-name")} required value={source.detectionName} onChange={(event) => onChange({ detectionName: event.target.value })} placeholder="7-Zip*" />
            </label>
          )}
          {source.detection === "file" && (
            <label htmlFor={id("detection-path")}>Path
              <input id={id("detection-path")} required value={source.detectionPath} onChange={(event) => onChange({ detectionPath: event.target.value })} placeholder={source.platform === "windows" ? "C:\\Program Files\\App\\app.exe" : "/Applications/App.app"} spellCheck={false} />
            </label>
          )}
          {source.detection === "registry" && (
            <label htmlFor={id("detection-key")}>Key
              <input id={id("detection-key")} required value={source.detectionKey} onChange={(event) => onChange({ detectionKey: event.target.value })} placeholder="HKLM\SOFTWARE\Vendor\App" spellCheck={false} />
            </label>
          )}
        </div>
        {source.detection === "application" && (
          <label htmlFor={id("detection-version")}>At least version
            <input id={id("detection-version")} value={source.detectionMinVersion} onChange={(event) => onChange({ detectionMinVersion: event.target.value })} placeholder="Any version" />
          </label>
        )}
        {source.detection === "registry" && (
          <label htmlFor={id("detection-value")}>With the value
            <input id={id("detection-value")} value={source.detectionValue} onChange={(event) => onChange({ detectionValue: event.target.value })} placeholder="Any value" spellCheck={false} />
          </label>
        )}
        <small className="field-help">{source.detection === "application" ? "As the device’s software list names it; * matches any text." : "The installer counts as installed while this exists."}</small>
      </>}
      {removable && <button type="button" className="link-button" onClick={onRemove}><Trash2 size={14} /> Remove this way</button>}
    </fieldset>
  );
}
