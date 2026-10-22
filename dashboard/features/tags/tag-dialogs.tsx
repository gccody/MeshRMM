import { LoaderCircle, Search, X } from "lucide-react";
import { type FormEvent, type RefObject, useId, useState } from "react";
import { AuthenticationRequired, errorText } from "../../lib/http";
import { ModalDialog } from "../../lib/modal-dialog";
import { type Agent, TAG_COLORS, type Tag, type TagColor } from "../agents/types";
import { useWorkspace } from "../workspace/workspace-context";
import { MAX_TAG_NAME_CHARS, TAG_COLOR_LABELS, type TagSummary, matchesTagName, tagChanges, tagCoverage, tagNameProblem } from "./model";
import { TagChip } from "./tag-chip";
import { changeDeviceTags, createTag, updateTag } from "./tags-api";

// The live inventory hears about a change from the event socket. While that
// is down, reading the list shows the change at once.
function useInventoryCatchUp() {
  const { inventory } = useWorkspace();
  return () => {
    if (inventory.connection !== "live") void inventory.loadAgents();
  };
}

// The color swatches to choose from.
function ColorChoice({ color, onChange, disabled }: { color: TagColor; onChange: (color: TagColor) => void; disabled: boolean }) {
  return (
    <fieldset className="tag-colors" disabled={disabled}>
      <legend>Color</legend>
      {TAG_COLORS.map((choice) => (
        <label key={choice} className={`tag-swatch tag-${choice}`} title={TAG_COLOR_LABELS[choice]}>
          <input type="radio" name="tag-color" value={choice} checked={color === choice} onChange={() => onChange(choice)} aria-label={TAG_COLOR_LABELS[choice]} />
          <i aria-hidden="true" />
        </label>
      ))}
    </fieldset>
  );
}

// Names and colors a new tag, or renames and recolors one.
export function TagDialog({ tag, returnFocus, onClose, onSaved }: {
  tag: Tag | null;
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
  onSaved?: (tag: TagSummary) => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const catchUp = useInventoryCatchUp();
  const titleId = useId();
  const [name, setName] = useState(tag?.name ?? "");
  const [color, setColor] = useState<TagColor>(tag?.color ?? "blue");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const problem = tagNameProblem(name, inventory.tags, tag?.id ?? null);
  const unchanged = tag !== null && name.trim() === tag.name && color === tag.color;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (problem) return;
    setBusy(true);
    setError(null);
    try {
      const saved = tag
        ? await updateTag(authorizedFetch, tag.id, { name: name.trim(), color })
        : await createTag(authorizedFetch, { name: name.trim(), color });
      catchUp();
      onSaved?.(saved);
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The tag could not be saved."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{tag ? `Edit ${tag.name}` : "New tag"}</h2>
      {!tag && <p>Tags group devices any way you like, such as by role, site or owner. A device can have many.</p>}
      <form onSubmit={(event) => void submit(event)}>
        <label>Name<input required maxLength={MAX_TAG_NAME_CHARS} value={name} disabled={busy} onChange={(event) => setName(event.target.value)} placeholder="Servers" /></label>
        <ColorChoice color={color} onChange={setColor} disabled={busy} />
        <p className="tag-preview"><TagChip tag={{ name: name.trim() || "Tag", color }} /></p>
        {(error || (problem && name.trim())) && <p role="alert" className="installer-error">{error ?? problem}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || problem !== null || unchanged}>{busy && <LoaderCircle size={16} className="spin" />} {tag ? "Save tag" : "Create tag"}</button>
        </div>
      </form>
    </ModalDialog>
  );
}

// Adds and removes tags on one device or many. A tag only some of the
// devices have shows as mixed until it is ticked or cleared.
export function TagDevicesDialog({ agents, returnFocus, onClose }: {
  agents: readonly Agent[];
  returnFocus: RefObject<HTMLElement | null>;
  onClose: () => void;
}) {
  const { authorizedFetch, inventory } = useWorkspace();
  const catchUp = useInventoryCatchUp();
  const titleId = useId();
  const [wanted, setWanted] = useState<ReadonlyMap<string, boolean>>(() => new Map());
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { tags } = inventory;
  const shown = tags.filter((tag) => matchesTagName(tag.name, query));
  const { add, remove } = tagChanges(agents, wanted);
  const one = agents.length === 1;
  const title = one ? `Tags of ${agents[0].name}` : `Tag ${agents.length} devices`;

  const stateOf = (tag: Tag) => {
    const chosen = wanted.get(tag.id);
    if (chosen !== undefined) return chosen ? "on" : "off";
    const coverage = tagCoverage(agents, tag.id);
    return coverage === "all" ? "on" : coverage === "none" ? "off" : "mixed";
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!add.length && !remove.length) return;
    setBusy(true);
    setError(null);
    try {
      await changeDeviceTags(authorizedFetch, { deviceIds: agents.map((agent) => agent.id), add, remove });
      catchUp();
      onClose();
    } catch (failure) {
      if (!(failure instanceof AuthenticationRequired)) setError(errorText(failure, "The tags could not be changed."));
      setBusy(false);
    }
  };

  return (
    <ModalDialog className="settings-modal" labelledBy={titleId} onClose={onClose} returnFocus={returnFocus}>
      <button type="button" className="modal-close" onClick={onClose} aria-label="Close"><X size={19} /></button>
      <h2 id={titleId}>{title}</h2>
      <p>{tags.length
        ? one ? "Tick the tags this device has." : "Tick a tag to add it to every chosen device, or clear it to take it off. A dash means only some have it; leave it to keep it that way."
        : "There are no tags yet. Someone with the tags.manage permission can create them on the Tags page."}</p>
      <form onSubmit={(event) => void submit(event)}>
        {tags.length > 8 && <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search tags" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search tags" /></label>}
        <fieldset className="choice-list picker-list" disabled={busy}>
          <legend className="sr-only">Tags</legend>
          {shown.map((tag) => (
            <label key={tag.id}>
              {/* Mixed: only some of the devices have the tag. */}
              <input
                type="checkbox"
                checked={stateOf(tag) === "on"}
                aria-checked={stateOf(tag) === "mixed" ? "mixed" : stateOf(tag) === "on"}
                ref={(box) => { if (box) box.indeterminate = stateOf(tag) === "mixed"; }}
                onChange={(event) => setWanted((current) => new Map(current).set(tag.id, event.target.checked))}
              />
              <TagChip tag={tag} />
            </label>
          ))}
          {tags.length > 0 && !shown.length && <p className="field-help">No tags match.</p>}
        </fieldset>
        {error && <p role="alert" className="installer-error">{error}</p>}
        <div className="connection-reason-actions">
          <button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
          <button className="primary-button" disabled={busy || (!add.length && !remove.length)}>{busy && <LoaderCircle size={16} className="spin" />} Save tags</button>
        </div>
      </form>
    </ModalDialog>
  );
}
