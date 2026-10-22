import { Search } from "lucide-react";
import { useMemo, useState } from "react";
import { type Agent, PLATFORM_LABELS } from "../agents/types";
import { LANGUAGE_LABELS, type ScriptLanguage, runsOn } from "../toolbox/model";

// A searchable list of devices to tick, noting those offline or without the
// script's interpreter.
export function DevicePicker({ agents, chosen, onChange, language }: {
  agents: readonly Agent[];
  chosen: readonly string[];
  onChange: (next: readonly string[]) => void;
  language: ScriptLanguage | null;
}) {
  const [query, setQuery] = useState("");
  const chosenSet = useMemo(() => new Set(chosen), [chosen]);
  const needle = query.trim().toLocaleLowerCase();
  const shown = agents.filter((agent) => !needle || agent.name.toLocaleLowerCase().includes(needle));
  const toggle = (id: string, checked: boolean) => onChange(checked ? [...chosen, id] : chosen.filter((candidate) => candidate !== id));
  const chooseShown = () => onChange([...new Set([...chosen, ...shown.map((agent) => agent.id)])]);
  return (
    <div className="device-picker">
      <div className="device-picker-tools">
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search devices" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search devices" /></label>
        <button type="button" className="link-button" onClick={chooseShown} disabled={!shown.length}>Choose all shown</button>
        <button type="button" className="link-button" onClick={() => onChange([])} disabled={!chosen.length}>Clear</button>
      </div>
      <fieldset className="choice-list picker-list">
        <legend className="sr-only">Devices to run on</legend>
        {shown.map((agent) => {
          const unsupported = language !== null && !runsOn(language, agent.platform);
          return (
            <label key={agent.id}>
              <input type="checkbox" checked={chosenSet.has(agent.id)} onChange={(event) => toggle(agent.id, event.target.checked)} />
              <span><strong>{agent.name}</strong>{PLATFORM_LABELS[agent.platform]}{agent.connected ? "" : " · Offline"}{unsupported && language ? ` · No ${LANGUAGE_LABELS[language]}, skipped` : ""}</span>
            </label>
          );
        })}
        {!shown.length && <p className="field-help">{agents.length ? "No devices match." : "There are no devices yet."}</p>}
      </fieldset>
    </div>
  );
}
