import { Search } from "lucide-react";
import { useState } from "react";
import type { Tag } from "../agents/types";
import { type TagMatch, matchesTagName } from "./model";

// A tag's name in its color.
export function TagChip({ tag }: { tag: Pick<Tag, "name" | "color"> }) {
  return <span className={`tag-chip tag-${tag.color}`} title={tag.name}><i aria-hidden="true" />{tag.name}</span>;
}

// A device's tags, the first `limit` and how many more.
export function TagChips({ tags, limit }: { tags: readonly Tag[]; limit?: number }) {
  if (!tags.length) return null;
  const shown = limit === undefined ? tags : tags.slice(0, limit);
  const more = tags.length - shown.length;
  return (
    <span className="tag-chips">
      <span className="sr-only">Tags: </span>
      {shown.map((tag) => <TagChip key={tag.id} tag={tag} />)}
      {more > 0 && <span className="tag-more" title={tags.slice(shown.length).map((tag) => tag.name).join(", ")}>+{more}</span>}
    </span>
  );
}

// Tags to tick, searchable once there are many.
export function TagChecklist({ tags, chosen, onChange, legend, disabled }: {
  tags: readonly Tag[];
  chosen: readonly string[];
  onChange: (next: string[]) => void;
  legend: string;
  disabled?: boolean;
}) {
  const [query, setQuery] = useState("");
  const shown = tags.filter((tag) => matchesTagName(tag.name, query));
  const toggle = (id: string, checked: boolean) => onChange(checked ? [...chosen, id] : chosen.filter((candidate) => candidate !== id));
  return (
    <div className="tag-checklist">
      {tags.length > 8 && <label className="search-field"><Search size={16} aria-hidden="true" /><input type="search" aria-label="Search tags" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search tags" /></label>}
      <fieldset className="choice-list picker-list" disabled={disabled}>
        <legend className="sr-only">{legend}</legend>
        {shown.map((tag) => (
          <label key={tag.id}>
            <input type="checkbox" checked={chosen.includes(tag.id)} onChange={(event) => toggle(tag.id, event.target.checked)} />
            <TagChip tag={tag} />
          </label>
        ))}
        {!shown.length && <p className="field-help">{tags.length ? "No tags match." : "There are no tags yet."}</p>}
      </fieldset>
    </div>
  );
}

// Whether devices need any or all of the chosen tags.
export function TagMatchChoice({ match, onChange, disabled }: { match: TagMatch; onChange: (match: TagMatch) => void; disabled?: boolean }) {
  return (
    <div className="segmented tag-match" role="group" aria-label="Devices need">
      <button type="button" aria-pressed={match === "any"} disabled={disabled} onClick={() => onChange("any")}>Any of these tags</button>
      <button type="button" aria-pressed={match === "all"} disabled={disabled} onClick={() => onChange("all")}>All of them</button>
    </div>
  );
}

// The tags a job, policy or rule targets, and whether devices need any or
// all of them.
export function TagTargetFields({ tags, chosen, match, onChange }: {
  tags: readonly Tag[];
  chosen: readonly string[];
  match: TagMatch;
  onChange: (change: { tagIds?: string[]; tagMatch?: TagMatch }) => void;
}) {
  return (
    <div className="tag-target">
      {chosen.length > 1 && <TagMatchChoice match={match} onChange={(tagMatch) => onChange({ tagMatch })} />}
      <TagChecklist tags={tags} chosen={chosen} onChange={(tagIds) => onChange({ tagIds })} legend="Tags" />
    </div>
  );
}
