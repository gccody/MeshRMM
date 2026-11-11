import type { Company, Tag } from "../agents/types";
import { TagTargetFields } from "../tags/tag-chip";
import type { ScopeDraft } from "./model";

// Which devices a report covers: every one, chosen companies' or tagged ones.
export function ScopeFields({ name, draft, companies, tags, onChange }: {
  name: string;
  draft: ScopeDraft;
  companies: readonly Company[];
  tags: readonly Tag[];
  onChange: (change: Partial<ScopeDraft>) => void;
}) {
  const toggle = (id: string, checked: boolean) => onChange({ companyIds: checked ? [...draft.companyIds, id] : draft.companyIds.filter((candidate) => candidate !== id) });
  return (
    <>
      <fieldset className="choice-list">
        <legend>Devices</legend>
        <label><input type="radio" name={name} checked={draft.target === "all"} onChange={() => onChange({ target: "all" })} /><span><strong>Every device</strong>As they are when the report is made.</span></label>
        <label><input type="radio" name={name} checked={draft.target === "companies"} onChange={() => onChange({ target: "companies" })} disabled={!companies.length && draft.target !== "companies"} /><span><strong>Devices of chosen companies</strong>{companies.length ? "Choose them below." : "There are no companies yet."}</span></label>
        <label><input type="radio" name={name} checked={draft.target === "tags"} onChange={() => onChange({ target: "tags" })} disabled={!tags.length && draft.target !== "tags"} /><span><strong>Devices with chosen tags</strong>{tags.length ? "The devices that have them when the report is made." : "There are no tags yet."}</span></label>
      </fieldset>
      {draft.target === "companies" && (
        <fieldset className="choice-list picker-list">
          <legend className="sr-only">Companies</legend>
          {companies.map((company) => (
            <label key={company.id}>
              <input type="checkbox" checked={draft.companyIds.includes(company.id)} onChange={(event) => toggle(company.id, event.target.checked)} />
              <strong>{company.name}</strong>
            </label>
          ))}
        </fieldset>
      )}
      {draft.target === "tags" && <TagTargetFields tags={tags} chosen={draft.tagIds} match={draft.tagMatch} onChange={({ tagIds, tagMatch }) => onChange({ ...(tagIds && { tagIds }), ...(tagMatch && { tagMatch }) })} />}
    </>
  );
}

// The zones a user can choose, with the current one kept when it's no
// longer listed.
export function ZoneSelect({ id, zones, value, onChange }: { id: string; zones: readonly string[]; value: string; onChange: (zone: string) => void }) {
  return (
    <label htmlFor={id}>Time zone
      <select id={id} value={value} onChange={(event) => onChange(event.target.value)}>
        {!zones.includes(value) && <option value={value}>{value}</option>}
        {zones.map((zone) => <option key={zone} value={zone}>{zone}</option>)}
      </select>
    </label>
  );
}
