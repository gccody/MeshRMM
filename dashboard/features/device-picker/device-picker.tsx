import { Search } from "lucide-react";
import { type ReactNode, useDeferredValue, useMemo, useRef, useState } from "react";
import { useWindowedRows } from "../../lib/use-windowed-rows";
import { type Agent, PLATFORM_LABELS } from "../agents/types";
import { LANGUAGE_LABELS, type ScriptLanguage, runsOn } from "../toolbox/model";
import { useWorkspace } from "../workspace/workspace-context";
import { DEVICE_ROW_HEIGHT, searchDevices } from "./model";

// A searchable list of devices to tick, noting those offline or without the
// script's interpreter. It draws only the rows on screen, so a fleet of a
// hundred thousand scrolls as easily as a few.
export function DevicePicker({ agents, chosen, onChange, language }: {
  agents: readonly Agent[];
  chosen: readonly string[];
  onChange: (next: readonly string[]) => void;
  language: ScriptLanguage | null;
}) {
  const { inventory } = useWorkspace();
  const [query, setQuery] = useState("");
  // Typing stays quick while a large fleet is searched.
  const search = useDeferredValue(query);
  const chosenSet = useMemo(() => new Set(chosen), [chosen]);
  const shown = useMemo(() => searchDevices(agents, search, inventory.tags), [agents, search, inventory.tags]);
  const scroller = useRef<HTMLDivElement>(null);
  const above = useRef<HTMLDivElement>(null);
  const below = useRef<HTMLDivElement>(null);
  // A new search starts at the top.
  const { first, last, onScroll } = useWindowedRows({ scroller, above, below }, shown.length, DEVICE_ROW_HEIGHT, search);
  const toggle = (id: string, checked: boolean) => onChange(checked ? [...chosen, id] : chosen.filter((candidate) => candidate !== id));
  const chooseShown = () => onChange([...new Set([...chosen, ...shown.map((agent) => agent.id)])]);

  const rows: ReactNode[] = [];
  for (let index = first; index < last; index++) {
    const agent = shown[index];
    const unsupported = language !== null && !runsOn(language, agent.platform);
    rows.push(
      <label key={agent.id}>
        <input type="checkbox" checked={chosenSet.has(agent.id)} onChange={(event) => toggle(agent.id, event.target.checked)} />
        <span><strong>{agent.name}</strong>{PLATFORM_LABELS[agent.platform]}{agent.connected ? "" : " · Offline"}{unsupported && language ? ` · No ${LANGUAGE_LABELS[language]}, skipped` : ""}</span>
      </label>,
    );
  }

  return (
    <div className="device-picker">
      <div className="device-picker-tools">
        <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search devices" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search devices" /></label>
        <button type="button" className="link-button" onClick={chooseShown} disabled={!shown.length}>Choose all shown</button>
        <button type="button" className="link-button" onClick={() => onChange([])} disabled={!chosen.length}>Clear</button>
      </div>
      <fieldset className="choice-list">
        <legend className="sr-only">Devices to run on</legend>
        <div ref={scroller} className="picker-list device-picker-rows" onScroll={onScroll}>
          <div ref={above} aria-hidden="true" />
          {rows}
          <div ref={below} aria-hidden="true" />
        </div>
        {!shown.length && <p className="field-help">{agents.length ? "No devices match." : "There are no devices yet."}</p>}
      </fieldset>
    </div>
  );
}
