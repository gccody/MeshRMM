import { X } from "lucide-react";
import type { RefObject } from "react";
import { ModalDialog } from "../../lib/modal-dialog";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import type { toolboxAccess } from "./access";
import { FileDetailsModal, UploadFilesModal } from "./file-dialogs";
import type { ScriptRun, ToolboxFile, ToolboxScript } from "./model";
import { RunResult, useScriptRun } from "./run-result";
import { RunScriptModal } from "./run-script-modal";
import { ScriptEditor } from "./script-editor";

export type ToolboxDialog =
  | { kind: "script"; script: ToolboxScript | null }
  | { kind: "run"; scriptId?: string }
  | { kind: "upload" }
  | { kind: "file"; file: ToolboxFile }
  | { kind: "run-details"; run: ScriptRun };

export function ToolboxDialogs({ dialog, access, scripts, folders, maxBytes, agents, returnFocus, onClose, onRunClosed, replaceScript, replaceFile }: {
  dialog: ToolboxDialog | null;
  access: ReturnType<typeof toolboxAccess>;
  scripts: ToolboxScript[];
  folders: string[];
  maxBytes: number | null;
  agents: Agent[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onRunClosed: () => void;
  replaceScript: (saved: ToolboxScript | null, id: string | null) => void;
  replaceFile: (saved: ToolboxFile | null, id: string) => void;
}) {
  return (
    <>
      {dialog?.kind === "script" && (
        <ScriptEditor
          script={dialog.script}
          sharing={access.shareScripts ? (access.keepPrivateScripts ? "optional" : "required") : "unavailable"}
          folders={folders}
          onClose={onClose}
          onSaved={(saved, id) => { replaceScript(saved, id); onClose(); }}
          returnFocus={returnFocus}
        />
      )}
      {dialog?.kind === "run" && (
        <RunScriptModal
          agents={agents}
          scripts={scripts}
          initialScriptId={dialog.scriptId}
          onClose={onRunClosed}
          returnFocus={returnFocus}
        />
      )}
      {dialog?.kind === "upload" && (
        <UploadFilesModal
          folders={folders}
          maxBytes={maxBytes}
          sharing={access.shareFiles ? (access.keepPrivateFiles ? "optional" : "required") : "unavailable"}
          onClose={onClose}
          onUploaded={(file) => replaceFile(file, file.id)}
          returnFocus={returnFocus}
        />
      )}
      {dialog?.kind === "file" && (
        <FileDetailsModal
          file={dialog.file}
          canShare={access.shareFiles}
          canKeepPrivate={access.keepPrivateFiles}
          folders={folders}
          onClose={onClose}
          onSaved={(saved, id) => { replaceFile(saved, id); onClose(); }}
          returnFocus={returnFocus}
        />
      )}
      {dialog?.kind === "run-details" && (
        <RunDetailsModal run={dialog.run} agents={agents} onClose={onClose} returnFocus={returnFocus} />
      )}
    </>
  );
}

function RunDetailsModal({ run: listed, agents, onClose, returnFocus }: { run: ScriptRun; agents: Agent[]; onClose: () => void; returnFocus: { current: HTMLElement | null } }) {
  const { authorizedFetch } = useWorkspace();
  const { run, error } = useScriptRun(authorizedFetch, listed);
  const deviceName = agents.find((agent) => agent.id === listed.device_id)?.name ?? listed.device_id;
  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy="run-details-title" onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id="run-details-title">{listed.script_name}</h2>
      {run && <RunResult run={run} deviceName={deviceName} error={error} />}
      <div className="connection-reason-actions"><button type="button" className="primary-button" onClick={onClose}>Done</button></div>
    </ModalDialog>
  );
}
