import { REBOOT_DELAYS, REBOOT_HELP, REBOOT_LABELS, type RebootBehavior, formatDelay } from "./model";

const BEHAVIORS: RebootBehavior[] = ["if_required", "never", "always"];

// When the device restarts after installing, and how long its user is
// warned first. Used by the install dialogs and the policy editor.
export function RebootFields({ name, reboot, delay, onReboot, onDelay }: {
  // Names the radio group, so two forms on a page don't share one.
  name: string;
  reboot: RebootBehavior;
  delay: number;
  onReboot: (reboot: RebootBehavior) => void;
  onDelay: (minutes: number) => void;
}) {
  const delays: number[] = REBOOT_DELAYS.includes(delay as (typeof REBOOT_DELAYS)[number]) ? [...REBOOT_DELAYS] : [...REBOOT_DELAYS, delay].sort((left, right) => left - right);
  return (
    <>
      <fieldset className="choice-list">
        <legend>Restart the device</legend>
        {BEHAVIORS.map((behavior) => (
          <label key={behavior}>
            <input type="radio" name={name} checked={reboot === behavior} onChange={() => onReboot(behavior)} />
            <span><strong>{REBOOT_LABELS[behavior]}</strong>{REBOOT_HELP[behavior]}</span>
          </label>
        ))}
      </fieldset>
      {reboot !== "never" && (
        <label htmlFor={`${name}-delay`}>Warn the signed-in user, then restart after
          <select id={`${name}-delay`} value={delay} onChange={(event) => onDelay(Number(event.target.value))}>
            {delays.map((minutes) => <option key={minutes} value={minutes}>{formatDelay(minutes)}</option>)}
          </select>
        </label>
      )}
      {reboot !== "never" && <small className="field-help">Windows shows a countdown with the warning; Linux and macOS send it to users signed in at a terminal. Unsaved work in open applications is lost when the time is up.</small>}
    </>
  );
}
