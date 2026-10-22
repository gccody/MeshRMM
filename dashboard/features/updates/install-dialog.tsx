import { Download, LoaderCircle, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { useWorkspace } from "../workspace/workspace-context";
import { DEFAULT_REBOOT_DELAY_MINUTES, type PendingUpdate, type RebootBehavior, type UpdateRun, rebootDelayProblem } from "./model";
import { RebootFields } from "./reboot-fields";
import { startInstall } from "./updates-api";

// Installs the chosen updates on one device, or all of them, and says when
// it may restart.
export function InstallDialog({ deviceId, deviceName, chosen, pendingCount, returnFocus, onClose, onStarted }: {
  deviceId: string;
  deviceName: string;
  // The chosen updates; empty installs every pending update.
  chosen: readonly PendingUpdate[];
  pendingCount: number;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onStarted: (run: UpdateRun) => void;
}) {
  const { authorizedFetch } = useWorkspace();
  const titleId = useId();
  const [reboot, setReboot] = useState<RebootBehavior>("if_required");
  const [delay, setDelay] = useState(DEFAULT_REBOOT_DELAY_MINUTES);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const all = chosen.length === 0;
  const count = all ? pendingCount : chosen.length;
  const problem = reboot === "never" ? null : rebootDelayProblem(delay);
  const needsRestart = chosen.some((update) => update.reboot_required);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      onStarted(await startInstall(authorizedFetch, deviceId, {
        updateIds: chosen.map((update) => update.id),
        categories: [],
        reboot,
        rebootDelayMinutes: delay,
      }));
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The install could not be started."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal toolbox-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{all ? "Install every pending update" : count === 1 ? "Install 1 update" : `Install ${count} updates`}</h2>
      <p>{deviceName} downloads and installs {all ? `its ${count === 1 ? "pending update" : `${count} pending updates`}` : count === 1 ? "it" : "them"} in the background, then looks for updates again. The Updates tab follows its progress.</p>
      {!all && (
        <ul className="install-chosen">
          {chosen.slice(0, 6).map((update) => <li key={update.id}>{update.title}</li>)}
          {chosen.length > 6 && <li className="more">and {chosen.length - 6} more</li>}
        </ul>
      )}
      <form onSubmit={(event) => void submit(event)}>
        <fieldset className="script-editor-fields" disabled={busy}>
          <RebootFields name="install-reboot" reboot={reboot} delay={delay} onReboot={setReboot} onDelay={setDelay} />
          {needsRestart && reboot === "never" && <p className="field-help">Some of these updates need a restart to finish. The device shows a restart as pending until it gets one.</p>}
        </fieldset>
        {(error || problem) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null}>
            {busy ? <LoaderCircle size={16} className="spin" /> : <Download size={16} />} Install
          </button>
        </div>
      </form>
    </ModalDialog>
  );
}
