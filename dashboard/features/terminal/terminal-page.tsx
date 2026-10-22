import { ArrowLeft, ClipboardCopy, ClipboardPaste, LoaderCircle, MonitorOff, Plus, RotateCw, Terminal, X } from "lucide-react";
import { type FormEvent, Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";
import { AuthenticationRequired, expectJson, jsonBody } from "../../lib/http";
import { usePopover } from "../../lib/use-popover";
import type { Agent } from "../agents/types";
import { HeaderActions } from "../workspace/header-actions";
import { useWorkspace } from "../workspace/workspace-context";
import {
  SHELL_LABELS,
  type TerminalAccount,
  type TerminalChoice,
  type TerminalShell,
  type TerminalStatus,
  accountLabel,
  accountOptions,
  initialChoice,
  isEnded,
  parseRememberedChoice,
  rememberedChoiceKey,
  shellOptions,
  statusText,
} from "./model";
import type { TerminalControls } from "./terminal-view";

// xterm.js is large, so it loads only when a terminal opens.
const TerminalView = lazy(() => import("./terminal-view"));

type Tab = TerminalChoice & {
  key: number;
  // Counts the shells started in this tab; Reconnect starts another.
  attempt: number;
  status: TerminalStatus;
};

const dotClass = (status: TerminalStatus) =>
  status.state === "connected" ? "online" : status.state === "connecting" ? "updating" : "offline";

// `/terminal?id=…`: shells on one device, each in its own tab. Closing a tab,
// or the page, ends its shell on the device.
export function TerminalPage() {
  const { inventory, authorizedFetch, can, setPageTitle } = useWorkspace();
  const [searchParams] = useSearchParams();
  const deviceId = searchParams.get("id") ?? "";
  const agent = inventory.agents.find((candidate) => candidate.id === deviceId);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeKey, setActiveKey] = useState<number | null>(null);
  const nextKey = useRef(1);
  // Each tab's copy and paste, which its terminal fills in once it loads.
  const [controls] = useState(() => new Map<number, { current: TerminalControls | null }>());
  const shells = useDeviceShells(agent);
  const accounts = accountOptions(can);
  // What the user picked; until then, the last choice on this kind of device.
  const [picked, setChoice] = useState<TerminalChoice | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const live = tabs.some((tab) => !isEnded(tab.status));

  useEffect(() => {
    setPageTitle(agent ? `Terminal on ${agent.name}` : null);
    return () => setPageTitle(null);
  }, [agent, setPageTitle]);

  // The workspace renders pages only in the browser, so storage is there.
  const platform = agent?.platform;
  const remembered = useMemo(() => {
    if (!platform) return null;
    try {
      return parseRememberedChoice(window.localStorage.getItem(rememberedChoiceKey(platform)));
    } catch {
      // Storage may be unavailable; start from the first choices.
      return null;
    }
  }, [platform]);
  const choice = shells ? initialChoice(shells, accounts, picked ?? remembered) : null;

  // Leaving the page ends its shells, so the browser asks first.
  useEffect(() => {
    if (!live) return;
    const warn = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [live]);

  const open = (picked: TerminalChoice) => {
    if (!agent) return;
    try {
      window.localStorage.setItem(rememberedChoiceKey(agent.platform), JSON.stringify(picked));
    } catch {
      // Not remembering is fine.
    }
    const key = nextKey.current++;
    setTabs((current) => [...current, { ...picked, key, attempt: 0, status: { state: "connecting" } }]);
    setActiveKey(key);
  };

  const close = (key: number) => {
    setTabs((current) => {
      const index = current.findIndex((tab) => tab.key === key);
      const rest = current.filter((tab) => tab.key !== key);
      if (key === activeKey) setActiveKey(rest[Math.min(index, rest.length - 1)]?.key ?? null);
      return rest;
    });
    controls.delete(key);
  };

  const update = useCallback((key: number, change: Partial<Tab>) => {
    setTabs((current) => current.map((tab) => (tab.key === key ? { ...tab, ...change } : tab)));
  }, []);

  const start = useCallback(async (tab: TerminalChoice, size: { columns: number; rows: number }) => {
    try {
      const response = await authorizedFetch(
        `/v1/agents/${encodeURIComponent(deviceId)}/terminals`,
        jsonBody({ shell: tab.shell, run_as: tab.account, columns: size.columns, rows: size.rows }),
      );
      const created = await expectJson<{ id: string }>(response, "The terminal couldn’t be opened.");
      return created.id;
    } catch (error) {
      throw error instanceof AuthenticationRequired ? new Error("Sign in again to open a terminal.") : error;
    }
  }, [authorizedFetch, deviceId]);

  if (inventory.hasData && !agent) {
    return (
      <div className="empty-state">
        <MonitorOff size={22} aria-hidden="true" />
        <strong>Device not found</strong>
        <span>It may have been deleted.</span>
        <Link className="secondary-button" to="/">Back to devices</Link>
      </div>
    );
  }

  const active = tabs.find((tab) => tab.key === activeKey) ?? null;
  const offline = Boolean(agent && !agent.connected);

  const copy = async () => {
    if (!active) return;
    try {
      const copied = await controls.get(active.key)?.current?.copy();
      setNotice(copied ? "Copied the selection." : "Select text in the terminal to copy it.");
    } catch {
      setNotice("The browser didn’t allow copying. Use the keyboard shortcut instead.");
    }
  };
  const paste = async () => {
    if (!active) return;
    try {
      await controls.get(active.key)?.current?.paste();
      setNotice(null);
    } catch {
      setNotice("The browser didn’t allow pasting. Use the keyboard shortcut instead.");
    }
  };

  return (
    <>
      {agent && tabs.length > 0 && (
        <HeaderActions>
          <NewTerminalMenu agent={agent} shells={shells} accounts={accounts} choice={choice} onChoose={setChoice} onOpen={open} disabled={offline} />
        </HeaderActions>
      )}
      <div className="terminal-page">
        <div className="device-status">
          <Link to={`/device?id=${encodeURIComponent(deviceId)}`} className="back-link"><ArrowLeft size={15} aria-hidden="true" />{agent?.name ?? "Device"}</Link>
          {agent && (
            <p className="status-line">
              <i className={`led ${agent.connected ? "online" : agent.updating_to ? "updating" : "offline"}`} aria-hidden="true" />
              <span>{agent.connected ? "Online" : agent.updating_to ? `Updating to ${agent.updating_to}` : "Offline"}</span>
              {agent.system?.os && <><span aria-hidden="true">·</span><span>{agent.system.os}</span></>}
            </p>
          )}
        </div>

        {!agent ? (
          <div className="empty-state compact"><LoaderCircle size={22} className="spin" aria-hidden="true" /><strong>Loading the device…</strong></div>
        ) : tabs.length === 0 ? (
          <section className="terminal-start" aria-labelledby="terminal-start-title">
            <Terminal size={22} aria-hidden="true" />
            <h2 id="terminal-start-title">Open a terminal</h2>
            <p>A shell on {agent.name}. It ends when you close its tab or this page.</p>
            {offline && <p role="status" className="terminal-offline">The device is offline. A terminal opens once it’s back online.</p>}
            <TerminalPicker agent={agent} shells={shells} accounts={accounts} choice={choice} onChoose={setChoice} onOpen={open} disabled={offline} />
          </section>
        ) : (
          <>
            <div className="terminal-tabs" role="tablist" aria-label="Terminals">
              {tabs.map((tab) => (
                <div key={tab.key} className="terminal-tab" data-active={tab.key === activeKey}>
                  <button type="button" role="tab" id={`terminal-tab-${tab.key}`} aria-selected={tab.key === activeKey} aria-controls={`terminal-panel-${tab.key}`} onClick={() => setActiveKey(tab.key)}>
                    <i className={`led ${dotClass(tab.status)}`} aria-hidden="true" />
                    <span>{SHELL_LABELS[tab.shell]}</span>
                    <em>{accountLabel(tab.account, agent.platform)}</em>
                    <span className="sr-only">, {statusText(tab.status)}</span>
                  </button>
                  <button type="button" className="terminal-tab-close" onClick={() => close(tab.key)} aria-label={`Close ${SHELL_LABELS[tab.shell]} as ${accountLabel(tab.account, agent.platform)}`} title="Close terminal"><X size={14} /></button>
                </div>
              ))}
            </div>
            {tabs.map((tab) => {
              let handle = controls.get(tab.key);
              if (!handle) {
                handle = { current: null };
                controls.set(tab.key, handle);
              }
              const shown = tab.key === activeKey;
              return (
                <section key={tab.key} id={`terminal-panel-${tab.key}`} className="terminal-panel" role="tabpanel" aria-labelledby={`terminal-tab-${tab.key}`} hidden={!shown}>
                  <div className="terminal-toolbar">
                    <p className={`terminal-status ${tab.status.state}`} role="status"><i className={`led ${dotClass(tab.status)}`} aria-hidden="true" />{statusText(tab.status)}</p>
                    {shown && notice && <p className="terminal-notice" role="status">{notice}</p>}
                    <span className="terminal-actions">
                      <button type="button" className="secondary-button" onClick={() => void copy()} title="Copy the selection (Ctrl+Shift+C, or Command-C on a Mac)"><ClipboardCopy size={15} aria-hidden="true" />Copy</button>
                      <button type="button" className="secondary-button" onClick={() => void paste()} disabled={tab.status.state !== "connected"} title="Paste (Ctrl+Shift+V, or Command-V on a Mac)"><ClipboardPaste size={15} aria-hidden="true" />Paste</button>
                      {isEnded(tab.status) && <button type="button" className="primary-button" disabled={offline} onClick={() => update(tab.key, { attempt: tab.attempt + 1, status: { state: "connecting" } })}><RotateCw size={15} aria-hidden="true" />Reconnect</button>}
                    </span>
                  </div>
                  <Suspense fallback={<div className="terminal-screen terminal-loading"><LoaderCircle size={18} className="spin" aria-hidden="true" />Loading terminal…</div>}>
                    <TerminalView
                      attempt={tab.attempt}
                      active={shown}
                      label={`${SHELL_LABELS[tab.shell]} as ${accountLabel(tab.account, agent.platform)} on ${agent.name}`}
                      start={(size) => start(tab, size)}
                      onStatus={(status) => update(tab.key, { status })}
                      onNotice={setNotice}
                      controls={handle}
                    />
                  </Suspense>
                </section>
              );
            })}
          </>
        )}
      </div>
    </>
  );
}

// The shells the device reported, or its platform's defaults until then.
function useDeviceShells(agent: Agent | undefined): TerminalShell[] | null {
  const { authorizedFetch } = useWorkspace();
  const [reported, setReported] = useState<{ id: string; shells: unknown } | null>(null);
  const id = agent?.id;
  const receivedAt = agent?.system?.received_at;
  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    authorizedFetch(`/v1/agents/${encodeURIComponent(id)}/system-info`)
      .then((response) => (response.ok ? response.json() : null))
      .then(
        (snapshot: { os?: { terminal_shells?: unknown } } | null) => {
          if (!cancelled) setReported({ id, shells: snapshot?.os?.terminal_shells ?? null });
        },
        () => {
          // Without it the platform's defaults are offered.
          if (!cancelled) setReported({ id, shells: null });
        },
      );
    return () => {
      cancelled = true;
    };
  }, [authorizedFetch, id, receivedAt]);
  if (!agent || reported?.id !== agent.id) return null;
  return shellOptions(agent.platform, reported.shells);
}

type PickerProps = {
  agent: Agent;
  shells: TerminalShell[] | null;
  accounts: TerminalAccount[];
  choice: TerminalChoice | null;
  onChoose: (choice: TerminalChoice) => void;
  onOpen: (choice: TerminalChoice) => void;
  disabled: boolean;
};

// The shell and account for a new terminal.
function TerminalPicker({ agent, shells, accounts, choice, onChoose, onOpen, disabled, idPrefix = "terminal" }: PickerProps & { idPrefix?: string }) {
  if (!shells || !choice) return <p role="status"><LoaderCircle size={15} className="spin" aria-hidden="true" /> Finding the device’s shells…</p>;
  const submit = (event: FormEvent) => {
    event.preventDefault();
    onOpen(choice);
  };
  return (
    <form className="terminal-picker" onSubmit={submit}>
      <label htmlFor={`${idPrefix}-shell`}>Shell
        <select id={`${idPrefix}-shell`} value={choice.shell} onChange={(event) => onChoose({ ...choice, shell: event.target.value as TerminalShell })}>
          {shells.map((shell) => <option key={shell} value={shell}>{SHELL_LABELS[shell]}</option>)}
        </select>
      </label>
      <fieldset className="run-as-choice">
        <legend>Run as</legend>
        {accounts.map((account) => (
          <label key={account}>
            <input type="radio" name={`${idPrefix}-account`} value={account} checked={choice.account === account} onChange={() => onChoose({ ...choice, account })} />
            <span><strong>{accountLabel(account, agent.platform)}</strong>{account === "user"
              ? "The person signed in to the device. Without one, the terminal doesn’t open."
              : "The device’s own account, with full control of the computer."}</span>
          </label>
        ))}
      </fieldset>
      <button className="primary-button" disabled={disabled}><Terminal size={16} aria-hidden="true" />Open terminal</button>
    </form>
  );
}

// Another terminal on the same device, beside the page's title.
function NewTerminalMenu(props: PickerProps) {
  const { open, setOpen, close, container, trigger } = usePopover();
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="primary-button" onClick={() => setOpen(!open)} aria-expanded={open} aria-controls={open ? "new-terminal" : undefined} disabled={props.disabled}>
        <Plus size={16} aria-hidden="true" />New terminal
      </button>
      {open && (
        <div id="new-terminal" className="popover terminal-popover" role="group" aria-label="New terminal">
          <TerminalPicker {...props} idPrefix="new-terminal" onOpen={(choice) => { close(); props.onOpen(choice); }} />
        </div>
      )}
    </div>
  );
}
