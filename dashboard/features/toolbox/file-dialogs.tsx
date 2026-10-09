
import { File as FileIcon, LoaderCircle, Upload, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useWorkspace } from "../workspace/workspace-context";
import type { Sharing } from "./access";
import { MAX_TOOLBOX_FILE_BYTES, type ToolboxFile, formatBytes, isValidFileName, normalizeFolder, uploadProblem } from "./model";
import { updateFile, uploadFile } from "./toolbox-api";

const FOLDER_PROBLEM = "Folders can be up to 8 levels deep, with names of up to 64 characters.";

/** Uploads files from this computer into one library folder. */
export function UploadFilesModal({ folders, initialFolder = "", maxBytes, sharing, onClose, onUploaded, returnFocus }: {
  folders: string[];
  initialFolder?: string;
  // The server's limit, once the toolbox has loaded.
  maxBytes: number | null;
  sharing: Sharing;
  onClose: () => void;
  onUploaded: (file: ToolboxFile) => void;
  returnFocus?: RefObject<HTMLElement | null>;
}) {
  const { authorizedFetch } = useWorkspace();
  const [files, setFiles] = useState<File[]>([]);
  const [folder, setFolder] = useState(initialFolder);
  const [shared, setShared] = useState(sharing === "required");
  const [progress, setProgress] = useState<string | null>(null);
  const [errors, setErrors] = useState<string[]>([]);
  const folderList = useId();
  const normalizedFolder = normalizeFolder(folder);
  const problems = files.map((file) => uploadProblem(file, maxBytes ?? MAX_TOOLBOX_FILE_BYTES));
  const uploadable = files.filter((_, index) => !problems[index]);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (normalizedFolder === null || !uploadable.length) return;
    const failures: string[] = [];
    for (const [index, file] of uploadable.entries()) {
      setProgress(`Uploading ${file.name} (${index + 1} of ${uploadable.length})…`);
      try {
        onUploaded(await uploadFile(authorizedFetch, file, normalizedFolder, shared));
      } catch (error) {
        if (error instanceof AuthenticationRequired) return;
        failures.push(error instanceof Error ? error.message : `${file.name} could not be uploaded.`);
      }
    }
    setProgress(null);
    if (failures.length) setErrors(failures);
    else onClose();
  };

  const uploading = progress !== null;
  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="upload-files-title" onClose={uploading ? () => {} : onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close" disabled={uploading}><X size={19} /></button>
      <h2 id="upload-files-title">Upload files</h2>
      <p>Technicians can send library files to a device from the toolbox in a remote session. Each file can be up to {formatBytes(maxBytes ?? MAX_TOOLBOX_FILE_BYTES)}.</p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={uploading}>
          <label htmlFor="upload-files">Files<input id="upload-files" type="file" multiple onChange={(event) => { setErrors([]); setFiles(Array.from(event.target.files ?? [])); }} /></label>
          {files.length > 0 && (
            <ul className="upload-list">
              {files.map((file, index) => (
                <li key={`${file.name}-${index}`} className={problems[index] ? "upload-problem" : undefined}>
                  <FileIcon size={15} aria-hidden="true" /><span>{file.name}</span><small>{problems[index] ?? formatBytes(file.size)}</small>
                </li>
              ))}
            </ul>
          )}
          <label htmlFor="upload-folder">Folder<input id="upload-folder" value={folder} list={folderList} placeholder="Top level" onChange={(event) => setFolder(event.target.value)} aria-invalid={normalizedFolder === null} /></label>
          <datalist id={folderList}>{folders.map((candidate) => <option key={candidate} value={candidate} />)}</datalist>
          {normalizedFolder === null && <small className="field-help">{FOLDER_PROBLEM}</small>}
          <label className="toolbox-share">
            <input type="checkbox" checked={shared} disabled={sharing !== "optional"} onChange={(event) => setShared(event.target.checked)} />
            <span><strong>Share with the whole company</strong>Everyone in the company can see and send them. Otherwise only you can.</span>
          </label>
        </fieldset>
        {progress && <p role="status" className="field-help"><LoaderCircle size={14} className="spin" /> {progress}</p>}
        {errors.map((message) => <p key={message} role="alert" className="installer-error">{message}</p>)}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose} disabled={uploading}>Cancel</button>
          <button className="primary-button" disabled={uploading || !uploadable.length || normalizedFolder === null}>
            {uploading ? <LoaderCircle size={16} className="spin" /> : <Upload size={16} />} Upload {uploadable.length > 1 ? `${uploadable.length} files` : "file"}
          </button>
        </div>
      </form>
    </ModalDialog>
  );
}

/** Renames, moves or shares a library file, or shows it to someone who can't. */
export function FileDetailsModal({ file, folders, canShare, canKeepPrivate, onClose, onSaved, returnFocus }: {
  file: ToolboxFile;
  folders: string[];
  canShare: boolean;
  canKeepPrivate: boolean;
  onClose: () => void;
  /** The saved file, or `null` when the user can no longer see it. */
  onSaved: (file: ToolboxFile | null, id: string) => void;
  returnFocus?: RefObject<HTMLElement | null>;
}) {
  const { authorizedFetch } = useWorkspace();
  const [name, setName] = useState(file.name);
  const [folder, setFolder] = useState(file.folder);
  const [shared, setShared] = useState(file.shared);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const folderList = useId();
  const readOnly = !file.can_edit;
  const normalizedFolder = normalizeFolder(folder);
  const nameValid = isValidFileName(name.trim());

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (readOnly || !nameValid || normalizedFolder === null) return;
    setSaving(true);
    setError(null);
    try {
      onSaved(await updateFile(authorizedFetch, file.id, { name: name.trim(), folder: normalizedFolder, shared }), file.id);
    } catch (saveError) {
      if (!(saveError instanceof AuthenticationRequired)) {
        setError(saveError instanceof Error ? saveError.message : "The file could not be saved.");
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy="file-details-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="file-details-title">{readOnly ? file.name : "File details"}</h2>
      <p>{formatBytes(file.size_bytes)} · SHA-256 <code className="file-digest">{file.sha256}</code></p>
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={readOnly || saving}>
          <label htmlFor="file-name">Name<input id="file-name" value={name} onChange={(event) => setName(event.target.value)} aria-invalid={!nameValid} /></label>
          {!nameValid && <small className="field-help">Use a name Windows allows: no \ / : * ? &quot; &lt; &gt; |, no trailing dot or space, and not a device name such as CON.</small>}
          <label htmlFor="file-folder">Folder<input id="file-folder" value={folder} list={folderList} placeholder="Top level" onChange={(event) => setFolder(event.target.value)} aria-invalid={normalizedFolder === null} /></label>
          <datalist id={folderList}>{folders.map((candidate) => <option key={candidate} value={candidate} />)}</datalist>
          {normalizedFolder === null && <small className="field-help">{FOLDER_PROBLEM}</small>}
          <label className="toolbox-share">
            <input type="checkbox" checked={shared} disabled={shared ? !canKeepPrivate : !canShare} onChange={(event) => setShared(event.target.checked)} />
            <span><strong>Share with the whole company</strong>Everyone in the company can see and send it.</span>
          </label>
        </fieldset>
        {readOnly && <p className="field-help">Only its owner can change this file, or someone who manages shared files while it is shared.</p>}
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>{readOnly ? "Close" : "Cancel"}</button>
          {!readOnly && <button className="primary-button" disabled={saving || !nameValid || normalizedFolder === null}>{saving && <LoaderCircle size={16} className="spin" />} Save</button>}
        </div>
      </form>
    </ModalDialog>
  );
}
