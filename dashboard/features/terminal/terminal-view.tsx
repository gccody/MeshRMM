// One terminal: xterm.js on a socket to a shell on the device. The page
// loads this module on demand, so xterm stays out of the main bundle.
import "@xterm/xterm/css/xterm.css";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { type ITheme, Terminal } from "@xterm/xterm";
import { type RefObject, useEffect, useImperativeHandle, useRef } from "react";
import {
  ACK_DELAY_MS,
  AckCounter,
  type TerminalStatus,
  ackMessage,
  clampSize,
  closeReason,
  PASTE_TOO_LARGE,
  inputMessages,
  isTooLarge,
  parseTerminalEvent,
  resizeMessage,
  terminalSocketUrl,
} from "./model";

// What the page's buttons do to a terminal.
export type TerminalControls = {
  copy: () => Promise<boolean>;
  paste: () => Promise<void>;
  focus: () => void;
};

type Props = {
  // A new value starts a new shell in the same view; there is no resuming.
  attempt: number;
  active: boolean;
  label: string;
  // Asks the server for a terminal of this size and returns its ID.
  start: (size: { columns: number; rows: number }) => Promise<string>;
  onStatus: (status: TerminalStatus) => void;
  // Something to tell the user that isn't the terminal's state.
  onNotice: (notice: string) => void;
  controls: RefObject<TerminalControls | null>;
};

// The site's dark console colors.
const THEME: ITheme = {
  background: "#07090d",
  foreground: "#e6e9ef",
  cursor: "#b0b7ff",
  cursorAccent: "#07090d",
  selectionBackground: "rgba(142, 152, 255, 0.35)",
  scrollbarSliderBackground: "rgba(163, 171, 187, 0.25)",
  scrollbarSliderHoverBackground: "rgba(163, 171, 187, 0.4)",
  scrollbarSliderActiveBackground: "rgba(163, 171, 187, 0.5)",
};

const RESIZE_DELAY_MS = 80;
const isMac = () => /Mac|iPhone|iPad/.test(navigator.platform);

// The site's Content-Security-Policy has no 'unsafe-inline', so the browser
// refuses (and reports) the <style> elements xterm adds for its scrollbar
// and, before the WebGL renderer takes over or when WebGL is unavailable,
// for its DOM renderer's layout and colors. While terminals are open, a
// <style> created through `document.createElement` becomes an inert
// <meshrmm-style> element (hidden by the site's stylesheet), and each one's
// rules are copied into a constructed stylesheet, which isn't an inline
// style, adopted by the document. Nothing else on the site creates <style>
// elements, since the policy would refuse them anyway.
const INERT_STYLE = "meshrmm-style";
let patched: { users: number; createElement: Document["createElement"] } | null = null;

function divertStyleElements() {
  if (!patched) {
    const createElement = document.createElement;
    patched = { users: 0, createElement };
    document.createElement = function (this: Document, tagName: string, options?: ElementCreationOptions) {
      return createElement.call(this, tagName.toLowerCase() === "style" ? INERT_STYLE : tagName, options);
    } as Document["createElement"];
  }
  patched.users += 1;
  return () => {
    if (!patched || --patched.users > 0) return;
    document.createElement = patched.createElement;
    patched = null;
  };
}

function adoptDivertedStyles(root: HTMLElement) {
  const sheets = new Map<Element, CSSStyleSheet>();
  const sync = () => {
    const present = new Set(root.querySelectorAll(INERT_STYLE));
    for (const [element, sheet] of sheets) {
      if (present.has(element)) continue;
      sheets.delete(element);
      document.adoptedStyleSheets = document.adoptedStyleSheets.filter((adopted) => adopted !== sheet);
    }
    for (const element of present) {
      let sheet = sheets.get(element);
      if (!sheet) {
        sheet = new CSSStyleSheet();
        sheets.set(element, sheet);
        document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
      }
      try {
        sheet.replaceSync(element.textContent ?? "");
      } catch {
        // A rule this browser can't parse; the rest of the terminal still works.
      }
    }
  };
  const observer = new MutationObserver(sync);
  observer.observe(root, { childList: true, subtree: true, characterData: true });
  return () => {
    observer.disconnect();
    const ours = new Set(sheets.values());
    document.adoptedStyleSheets = document.adoptedStyleSheets.filter((sheet) => !ours.has(sheet));
  };
}

export default function TerminalView({ attempt, active, label, start, onStatus, onNotice, controls }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const socket = useRef<WebSocket | null>(null);
  // The size last sent, so a resize goes out only when it changes.
  const sent = useRef<{ columns: number; rows: number } | null>(null);
  // Callbacks can change between renders; the terminal's handlers read these.
  const latest = useRef({ start, onStatus, onNotice, active });
  useEffect(() => {
    latest.current = { start, onStatus, onNotice, active };
  });

  useImperativeHandle(controls, () => ({
    copy: async () => {
      const selection = terminal.current?.getSelection() ?? "";
      if (!selection) return false;
      await navigator.clipboard.writeText(selection);
      return true;
    },
    paste: async () => {
      const text = await navigator.clipboard.readText();
      if (text) terminal.current?.paste(text);
      terminal.current?.focus();
    },
    focus: () => terminal.current?.focus(),
  }), []);

  // The terminal itself, for as long as the view is open.
  useEffect(() => {
    const element = container.current;
    if (!element) return;
    const restoreStyles = divertStyleElements();
    const releaseStyles = adoptDivertedStyles(element);
    const term = new Terminal({
      fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", monospace',
      fontSize: 13,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 5000,
      theme: THEME,
      // Option+key sends Meta on macOS, as Terminal does with that setting.
      macOptionIsMeta: true,
    });
    const fitter = new FitAddon();
    term.loadAddon(fitter);
    term.open(element);
    try {
      const webgl = new WebglAddon();
      // A lost GPU context falls back to the DOM renderer.
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // No WebGL: the DOM renderer, styled through adoptDivertedStyles.
    }
    terminal.current = term;
    fit.current = fitter;

    const send = (data: string | Uint8Array) => {
      const open = socket.current;
      if (open?.readyState === WebSocket.OPEN) open.send(data);
    };
    // A paste arrives as one piece of input; one too large is refused whole.
    const sendInput = (messages: Uint8Array[]) => {
      if (isTooLarge(messages)) {
        latest.current.onNotice(PASTE_TOO_LARGE);
        return;
      }
      messages.forEach((message) => send(message));
    };
    const input = term.onData((data) => sendInput(inputMessages(data)));
    const binary = term.onBinary((data) => sendInput(inputMessages(data, true)));
    const resized = term.onResize(({ cols, rows }) => {
      if (sent.current?.columns === cols && sent.current.rows === rows) return;
      if (socket.current?.readyState !== WebSocket.OPEN) return;
      sent.current = { columns: cols, rows };
      send(resizeMessage(cols, rows));
    });
    // Ctrl+Shift+C and Ctrl+Shift+V copy and paste on Windows and Linux,
    // where Ctrl+C belongs to the shell; macOS uses Command as everywhere.
    term.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown" || isMac() || !event.ctrlKey || !event.shiftKey) return true;
      const key = event.key.toLowerCase();
      if (key === "c") {
        const selection = term.getSelection();
        if (selection) void navigator.clipboard.writeText(selection).catch(() => {});
        return false;
      }
      if (key === "v") {
        event.preventDefault();
        void navigator.clipboard.readText().then((text) => text && term.paste(text), () => {});
        return false;
      }
      return true;
    });

    // Fits the terminal to its box when the page lays it out again. A
    // hidden tab measures nothing, so it fits when it is shown.
    let timer: number | undefined;
    const observer = new ResizeObserver(() => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        if (element.clientWidth > 0 && element.clientHeight > 0) fitter.fit();
      }, RESIZE_DELAY_MS);
    });
    observer.observe(element);

    return () => {
      window.clearTimeout(timer);
      observer.disconnect();
      input.dispose();
      binary.dispose();
      resized.dispose();
      socket.current?.close(1000);
      socket.current = null;
      terminal.current = null;
      fit.current = null;
      term.dispose();
      releaseStyles();
      restoreStyles();
    };
  }, []);

  // A shell for each attempt: ask the server for it, then connect.
  useEffect(() => {
    const term = terminal.current;
    if (!term) return;
    let cancelled = false;
    let ended = false;
    let ws: WebSocket | null = null;
    let ackTimer: number | undefined;
    const report = (status: TerminalStatus) => {
      if (cancelled) return;
      if (status.state !== "connecting" && status.state !== "connected") ended = true;
      latest.current.onStatus(status);
    };
    const note = (text: string) => term.write(`\r\n\x1b[2m${text}\x1b[0m\r\n`);
    if (attempt > 0) {
      note("Starting a new shell…");
      // Reconnect's button goes away; the keyboard goes to the new shell.
      if (latest.current.active) term.focus();
    }
    report({ state: "connecting" });

    const proposed = fit.current?.proposeDimensions();
    const size = clampSize(proposed?.cols ?? term.cols, proposed?.rows ?? term.rows);
    if (proposed) term.resize(size.columns, size.rows);
    void latest.current.start(size).then(
      (id) => {
        if (cancelled) return;
        ws = new WebSocket(terminalSocketUrl(window.location, id));
        ws.binaryType = "arraybuffer";
        socket.current = ws;
        sent.current = size;
        const acks = new AckCounter();
        const acknowledge = (bytes: number | null) => {
          if (bytes !== null && ws?.readyState === WebSocket.OPEN) ws.send(ackMessage(bytes));
        };
        ws.onmessage = (message) => {
          if (message.data instanceof ArrayBuffer) {
            const bytes = new Uint8Array(message.data);
            // Acknowledged once drawn, so a flood waits on the device.
            term.write(bytes, () => {
              acknowledge(acks.drawn(bytes.length));
              window.clearTimeout(ackTimer);
              if (acks.pending > 0) ackTimer = window.setTimeout(() => acknowledge(acks.flush()), ACK_DELAY_MS);
            });
            return;
          }
          const event = parseTerminalEvent(String(message.data));
          if (event?.type === "started") {
            report({ state: "connected", account: event.account, shell: event.shell });
            // The layout may have changed while the shell started.
            if (latest.current.active) fit.current?.fit();
          } else if (event?.type === "failed") {
            note(`Couldn’t start the shell: ${event.message}`);
            report({ state: "failed", message: event.message });
          } else if (event?.type === "exited") {
            note(event.code === null ? "The shell exited." : `The shell exited with code ${event.code}.`);
            report({ state: "exited", code: event.code });
          }
        };
        ws.onclose = (close) => {
          window.clearTimeout(ackTimer);
          if (socket.current === ws) socket.current = null;
          if (ended || cancelled) return;
          const reason = closeReason(close.code, close.reason);
          note(`Disconnected: ${reason}`);
          report({ state: "disconnected", reason });
        };
      },
      (error: unknown) => {
        const message = error instanceof Error ? error.message : "The terminal couldn’t be opened.";
        note(message);
        report({ state: "failed", message });
      },
    );
    return () => {
      cancelled = true;
      window.clearTimeout(ackTimer);
      ws?.close(1000);
    };
  }, [attempt]);

  // A tab shown again fits its box and takes the keyboard.
  useEffect(() => {
    if (!active) return;
    const element = container.current;
    if (element && element.clientWidth > 0) fit.current?.fit();
    terminal.current?.focus();
  }, [active]);

  return <div ref={container} className="terminal-screen" role="group" aria-label={label} />;
}
