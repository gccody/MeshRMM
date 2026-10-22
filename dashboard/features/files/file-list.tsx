import { ArrowDown, ArrowUp, File as FileIcon, FileQuestion, Folder, FolderSymlink, Link2, MoreHorizontal } from "lucide-react";
import { type DragEvent, type KeyboardEvent, type MouseEvent, type ReactNode, useEffect, useLayoutEffect, useRef, useState } from "react";
import { usePopover } from "../../lib/use-popover";
import { formatBytes } from "../metrics/model";
import { type FileEntry, type FileSort, ROW_HEIGHT, typeLabel, visibleRows } from "./model";
import { draggingFiles } from "./picked";

export type RowAction = {
  label: string;
  icon: ReactNode;
  onSelect: () => void;
  danger?: boolean;
  disabled?: boolean;
};

type Props = {
  // Changes whenever the listing starts over: another folder or order.
  listKey: string;
  total: number;
  entryAt: (index: number) => FileEntry | undefined;
  sort: FileSort;
  descending: boolean;
  onSort: (sort: FileSort) => void;
  selected: ReadonlySet<string>;
  onSelect: (index: number, mode: "only" | "toggle" | "range") => void;
  allSelected: boolean;
  onSelectAll: (all: boolean) => void;
  onOpen: (entry: FileEntry) => void;
  actionsFor: (entry: FileEntry) => RowAction[];
  // The rows on screen, to load the pages they're on.
  onVisible: (first: number, last: number) => void;
  // Files dropped on the list, or on a folder in it.
  onDropFiles?: (transfer: DataTransfer, into: FileEntry | null) => void;
  onKeyDown?: (event: KeyboardEvent<HTMLElement>) => void;
  label: string;
};

const COLUMNS: { sort: FileSort; label: string; className: string }[] = [
  { sort: "name", label: "Name", className: "file-name-column" },
  { sort: "size", label: "Size", className: "file-size-column" },
  { sort: "modified", label: "Modified", className: "file-modified-column" },
  { sort: "type", label: "Type", className: "file-type-column" },
];

const modifiedText = (modified?: number) =>
  modified === undefined
    ? ""
    : new Date(modified).toLocaleString([], { year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

function EntryIcon({ entry }: { entry: FileEntry }) {
  if (entry.kind === "directory") return entry.link ? <FolderSymlink size={16} aria-hidden="true" /> : <Folder size={16} aria-hidden="true" />;
  if (entry.kind === "other") return entry.link ? <Link2 size={16} aria-hidden="true" /> : <FileQuestion size={16} aria-hidden="true" />;
  return <FileIcon size={16} aria-hidden="true" />;
}

// A folder's entries in a table that draws only the rows on screen, so a
// folder of tens of thousands scrolls as easily as a short one.
export function FileList(props: Props) {
  const { listKey, total, entryAt, selected, onVisible } = props;
  const scroller = useRef<HTMLDivElement>(null);
  const above = useRef<HTMLTableRowElement>(null);
  const below = useRef<HTMLTableRowElement>(null);
  // Where the list is scrolled to, in the listing it was scrolled in.
  const [scroll, setView] = useState({ key: listKey, top: 0, height: 600 });
  const view = scroll.key === listKey ? scroll : { ...scroll, top: 0 };
  const [dropTarget, setDropTarget] = useState<number | "list" | null>(null);
  const frame = useRef(0);
  const { first, last } = visibleRows(view.top, view.height, total);

  useLayoutEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const observer = new ResizeObserver(() => setView((current) => ({ ...current, height: node.clientHeight })));
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  // A new listing starts at the top.
  useLayoutEffect(() => {
    if (scroller.current) scroller.current.scrollTop = 0;
  }, [listKey]);

  // The space of the rows not drawn, set directly: the site's content
  // security policy keeps style attributes out of the markup.
  useLayoutEffect(() => {
    if (above.current) above.current.style.height = `${first * ROW_HEIGHT}px`;
    if (below.current) below.current.style.height = `${Math.max(0, total - last) * ROW_HEIGHT}px`;
  }, [first, last, total]);

  useEffect(() => {
    onVisible(first, last);
  }, [first, last, onVisible, listKey]);

  const scrolled = () => {
    cancelAnimationFrame(frame.current);
    frame.current = requestAnimationFrame(() => {
      const node = scroller.current;
      if (node) setView({ key: listKey, top: node.scrollTop, height: node.clientHeight });
    });
  };

  const focusRow = (index: number) => {
    const node = scroller.current;
    if (!node || index < 0 || index >= total) return;
    const top = index * ROW_HEIGHT;
    const header = ROW_HEIGHT;
    if (top < node.scrollTop) node.scrollTop = top;
    else if (top + ROW_HEIGHT > node.scrollTop + node.clientHeight - header) node.scrollTop = top + ROW_HEIGHT * 2 - node.clientHeight;
    requestAnimationFrame(() => node.querySelector<HTMLElement>(`[data-index="${index}"]`)?.focus());
  };

  const rowKeyDown = (event: KeyboardEvent<HTMLTableRowElement>, index: number, entry: FileEntry | undefined) => {
    if (event.target !== event.currentTarget) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const next = index + (event.key === "ArrowDown" ? 1 : -1);
      focusRow(next);
      if (next >= 0 && next < total) props.onSelect(next, event.shiftKey ? "range" : "only");
      return;
    }
    if (!entry) return;
    if (event.key === "Enter") {
      event.preventDefault();
      props.onOpen(entry);
    } else if (event.key === " ") {
      event.preventDefault();
      props.onSelect(index, "toggle");
    } else {
      props.onKeyDown?.(event);
    }
  };

  const rowClick = (event: MouseEvent<HTMLTableRowElement>, index: number) => {
    const target = event.target as HTMLElement;
    if (target.closest("button, input, a")) return;
    props.onSelect(index, event.shiftKey ? "range" : event.metaKey || event.ctrlKey ? "toggle" : "only");
  };

  // Files from the computer drop into the folder shown, or onto a folder
  // in it.
  const dropIndex = (event: DragEvent) => {
    const row = (event.target as HTMLElement).closest<HTMLElement>("[data-index]");
    const index = row ? Number(row.dataset.index) : null;
    return index !== null && entryAt(index)?.kind === "directory" ? index : "list";
  };
  const dragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!props.onDropFiles || !draggingFiles(event.dataTransfer)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
    setDropTarget(dropIndex(event));
  };
  const drop = (event: DragEvent<HTMLDivElement>) => {
    if (!props.onDropFiles || !draggingFiles(event.dataTransfer)) return;
    event.preventDefault();
    const target = dropIndex(event);
    setDropTarget(null);
    props.onDropFiles(event.dataTransfer, target === "list" ? null : entryAt(target) ?? null);
  };

  const rows: ReactNode[] = [];
  for (let index = first; index < last; index++) {
    const entry = entryAt(index);
    const isSelected = entry ? selected.has(entry.name) : false;
    rows.push(
      <tr
        key={entry ? `e:${entry.name}` : `i:${index}`}
        className={`file-row${isSelected ? " selected" : ""}${entry?.hidden || entry?.system ? " dimmed" : ""}${dropTarget === index ? " drop-target" : ""}`}
        data-index={index}
        tabIndex={0}
        aria-selected={isSelected}
        onClick={(event) => rowClick(event, index)}
        onDoubleClick={() => entry && props.onOpen(entry)}
        onKeyDown={(event) => rowKeyDown(event, index, entry)}
      >
        <td className="file-check-column">
          {entry && <input type="checkbox" checked={isSelected} onChange={() => props.onSelect(index, "toggle")} aria-label={`Select ${entry.name}`} tabIndex={-1} />}
        </td>
        {entry ? (
          <>
            <td className="file-name-column">
              <span className={`file-entry-name ${entry.kind}`} title={entry.name}><EntryIcon entry={entry} /><span>{entry.name}</span></span>
            </td>
            <td className="file-size-column">{entry.size !== undefined ? formatBytes(entry.size) : ""}</td>
            <td className="file-modified-column">{modifiedText(entry.modified)}</td>
            <td className="file-type-column">{typeLabel(entry)}{entry.read_only ? " · read-only" : ""}</td>
            <td className="file-menu-column"><RowMenu entry={entry} actions={props.actionsFor(entry)} /></td>
          </>
        ) : (
          <td colSpan={5} className="file-loading-row"><span aria-hidden="true" />Loading…</td>
        )}
      </tr>,
    );
  }

  return (
    <div
      ref={scroller}
      className={`file-list${dropTarget === "list" ? " drop-target" : ""}`}
      onScroll={scrolled}
      onDragOver={dragOver}
      onDragLeave={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDropTarget(null);
      }}
      onDrop={drop}
    >
      <table className="file-table" aria-label={props.label} aria-rowcount={total + 1} aria-multiselectable="true">
        <thead>
          <tr>
            <th scope="col" className="file-check-column">
              <input type="checkbox" checked={props.allSelected} disabled={total === 0} onChange={(event) => props.onSelectAll(event.target.checked)} aria-label="Select everything shown" />
            </th>
            {COLUMNS.map((column) => {
              const sorted = props.sort === column.sort;
              return (
                <th key={column.sort} scope="col" className={column.className} aria-sort={sorted ? (props.descending ? "descending" : "ascending") : "none"}>
                  <button type="button" className="file-sort" onClick={() => props.onSort(column.sort)}>
                    {column.label}
                    {sorted && (props.descending ? <ArrowDown size={13} aria-hidden="true" /> : <ArrowUp size={13} aria-hidden="true" />)}
                  </button>
                </th>
              );
            })}
            <th scope="col" className="file-menu-column"><span className="sr-only">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          <tr ref={above} className="file-spacer" aria-hidden="true"><td colSpan={6} /></tr>
          {rows}
          <tr ref={below} className="file-spacer" aria-hidden="true"><td colSpan={6} /></tr>
        </tbody>
      </table>
    </div>
  );
}

function RowMenu({ entry, actions }: { entry: FileEntry; actions: RowAction[] }) {
  const { open, setOpen, close, container, trigger } = usePopover();
  if (!actions.length) return null;
  return (
    <div className="popover-anchor" ref={container}>
      <button ref={trigger} type="button" className="icon-button ghost" onClick={() => setOpen(!open)} aria-expanded={open} aria-label={`Actions for ${entry.name}`} tabIndex={-1}>
        <MoreHorizontal size={16} />
      </button>
      {open && (
        <div className="popover menu file-row-menu" role="group" aria-label={`Actions for ${entry.name}`}>
          {actions.map((action) => (
            <button key={action.label} type="button" className={action.danger ? "danger" : undefined} disabled={action.disabled} onClick={() => { close(); action.onSelect(); }}>
              {action.icon}{action.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
