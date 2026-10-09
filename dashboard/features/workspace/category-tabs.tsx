import type { KeyboardEvent } from "react";
import { tabForKey } from "../settings/general-settings";

// A page's categories as a tab list beside the chosen one: a column on wide
// screens, a row on narrow ones.
export function CategoryTabs<Id extends string>({ label, idPrefix, tabs, selected, onSelect }: {
  label: string;
  idPrefix: string;
  tabs: readonly { id: Id; label: string }[];
  selected: Id;
  onSelect: (id: Id) => void;
}) {
  const onKeyDown = (index: number) => (event: KeyboardEvent<HTMLButtonElement>) => {
    const target = tabForKey(index, tabs.length, event.key);
    if (target === null) return;
    event.preventDefault();
    const next = tabs[target].id;
    onSelect(next);
    document.getElementById(`${idPrefix}-tab-${next}`)?.focus();
  };
  return (
    <div className="settings-categories" role="tablist" aria-label={label} aria-orientation="vertical">
      {tabs.map((tab, index) => (
        <button
          key={tab.id}
          type="button"
          role="tab"
          id={`${idPrefix}-tab-${tab.id}`}
          aria-controls={`${idPrefix}-${tab.id}`}
          aria-selected={selected === tab.id}
          tabIndex={selected === tab.id ? 0 : -1}
          onClick={() => onSelect(tab.id)}
          onKeyDown={onKeyDown(index)}
        >{tab.label}</button>
      ))}
    </div>
  );
}
