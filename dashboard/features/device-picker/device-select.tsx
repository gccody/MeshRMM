import { Check, ChevronDown, Search } from "lucide-react";
import { type KeyboardEvent, useDeferredValue, useEffect, useId, useMemo, useRef, useState } from "react";
import { usePopover } from "../../lib/use-popover";
import type { Agent } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { matchDevices, moreMatchesText } from "./model";

type Props = {
  agents: readonly Agent[];
  // The chosen device's ID, or "" for none.
  value: string;
  onChange: (id: string) => void;
  // What the control is, such as "Device"; it names the button with its value.
  label: string;
  // The button's id, for a <label htmlFor>.
  id?: string;
  // An option for no device, such as "Every device".
  none?: string;
  // What shows while no device is chosen, when there's no such option.
  placeholder?: string;
  // What follows a device's name in brackets, such as its platform.
  describe?: (agent: Agent) => string;
  // Devices listed but not choosable.
  disabled?: (agent: Agent) => boolean;
};

// Chooses one device from however many: a button showing the choice that
// opens a search over the fleet, listing the first matches.
export function DeviceSelect({ agents, value, onChange, label, id, none, placeholder, describe, disabled }: Props) {
  const { open, setOpen, close, container, trigger } = usePopover();
  const menuId = useId();
  const current = useMemo(() => (value ? agents.find((agent) => agent.id === value) : undefined), [agents, value]);
  const nameOf = (agent: Agent) => (describe ? `${agent.name} (${describe(agent)})` : agent.name);
  // A link may name a device this browser doesn't know.
  const text = current ? nameOf(current) : value ? "This device" : none ?? placeholder ?? "";
  const choose = (next: string) => {
    close();
    if (next !== value) onChange(next);
  };

  return (
    <div className="popover-anchor device-select" ref={container}>
      <button ref={trigger} id={id} type="button" className="device-select-trigger" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? menuId : undefined} aria-label={`${label}: ${text}`}>
        <span>{text}</span><ChevronDown size={14} aria-hidden="true" />
      </button>
      {open && <DeviceSelectMenu id={menuId} label={label} agents={agents} value={value} unknown={Boolean(value && !current)} none={none} nameOf={nameOf} disabled={disabled} onChoose={choose} />}
    </div>
  );
}

// The open menu, with a search box that starts empty each time.
function DeviceSelectMenu({ id, label, agents, value, unknown, none, nameOf, disabled, onChoose }: {
  id: string;
  label: string;
  agents: readonly Agent[];
  value: string;
  unknown: boolean;
  none?: string;
  nameOf: (agent: Agent) => string;
  disabled?: (agent: Agent) => boolean;
  onChoose: (id: string) => void;
}) {
  const { inventory } = useWorkspace();
  const [query, setQuery] = useState("");
  // Typing stays quick while a large fleet is searched.
  const search = useDeferredValue(query);
  const list = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.focus(), []);
  const { shown, total } = useMemo(() => matchDevices(agents, search, inventory.tags), [agents, search, inventory.tags]);
  const searching = Boolean(search.trim());
  const more = moreMatchesText(shown.length, total);

  // The arrow keys move between the search box and the options. Escape is
  // the menu's, not the dialog's around it, which usePopover closes.
  const keyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const items = [input.current, ...(list.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
    const index = items.indexOf(document.activeElement as HTMLInputElement | HTMLButtonElement);
    const next = items[Math.min(items.length - 1, Math.max(0, index + (event.key === "ArrowDown" ? 1 : -1)))];
    if (!next) return;
    event.preventDefault();
    next.focus();
  };

  return (
    <div id={id} className="popover menu device-select-menu" role="group" aria-label={label}>
      <label className="menu-search"><Search size={15} aria-hidden="true" />
        <input
          ref={input}
          type="search"
          aria-label="Search devices"
          placeholder="Search devices"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            // Enter takes the first match rather than submitting a form.
            if (event.key === "Enter") {
              event.preventDefault();
              const first = searching ? shown.find((agent) => !disabled?.(agent)) : undefined;
              if (first) onChoose(first.id);
            } else {
              keyDown(event);
            }
          }}
        />
      </label>
      <div ref={list} className="menu-options">
        {!searching && none !== undefined && (
          <button type="button" aria-pressed={value === ""} onClick={() => onChoose("")} onKeyDown={keyDown}>
            <Check size={16} aria-hidden="true" className={value === "" ? undefined : "invisible"} /><span>{none}</span>
          </button>
        )}
        {!searching && unknown && (
          <button type="button" aria-pressed="true" onClick={() => onChoose(value)} onKeyDown={keyDown}>
            <Check size={16} aria-hidden="true" /><span>This device</span>
          </button>
        )}
        {shown.map((agent) => (
          <button key={agent.id} type="button" aria-pressed={agent.id === value} disabled={disabled?.(agent)} onClick={() => onChoose(agent.id)} onKeyDown={keyDown}>
            <Check size={16} aria-hidden="true" className={agent.id === value ? undefined : "invisible"} /><span>{nameOf(agent)}</span>
          </button>
        ))}
        {(more || !shown.length) && <p className="menu-empty" role="status">{more ?? (agents.length ? "No matching devices" : "No devices yet")}</p>}
      </div>
    </div>
  );
}
