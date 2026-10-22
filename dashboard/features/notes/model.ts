// Devices' notes. Imports carry the .ts extension so node:test can load this
// module directly.

export const MAX_NOTES_CHARS = 20_000;

// A kept revision of a device's notes, without its text.
export type NotesRevision = {
  revision: number;
  updated_at: number;
  updated_by_user_id: string | null;
  updated_by: string | null;
  length: number;
};

export type Notes = {
  // Empty without notes.
  body: string;
  // 0 without notes.
  revision: number;
  updated_at: number | null;
  updated_by_user_id: string | null;
  updated_by: string | null;
  // Newest first, the current revision included.
  history: NotesRevision[];
};

export type RevisionText = NotesRevision & { body: string };

// A piece of notes text: plain text, or a web address to link.
export type NotesPart = { kind: "text"; text: string } | { kind: "link"; text: string };

const LINK = /\bhttps?:\/\/[^\s<>"']+/giu;
// Punctuation that ends a sentence rather than an address.
const TRAILING = /[.,;:!?)\]}]+$/u;

// Splits notes into text and http(s) links, so the page can render links as
// elements and everything else as text, never as HTML.
export function notesParts(text: string): NotesPart[] {
  const parts: NotesPart[] = [];
  let from = 0;
  for (const match of text.matchAll(LINK)) {
    let link = match[0];
    const trailing = TRAILING.exec(link)?.[0] ?? "";
    // A closing bracket the address opened belongs to it.
    const keep = trailing.startsWith(")") && link.includes("(") ? 1 : 0;
    link = link.slice(0, link.length - trailing.length + keep);
    const start = match.index ?? 0;
    if (start > from) parts.push({ kind: "text", text: text.slice(from, start) });
    parts.push({ kind: "link", text: link });
    from = start + link.length;
  }
  if (from < text.length) parts.push({ kind: "text", text: text.slice(from) });
  return parts;
}

export const notesLength = (text: string) => Array.from(text).length;

// Why the text can't be saved, or null.
export function notesProblem(text: string) {
  const length = notesLength(text);
  return length > MAX_NOTES_CHARS ? `Notes can be at most ${MAX_NOTES_CHARS.toLocaleString()} characters; these are ${length.toLocaleString()}.` : null;
}

export const editorName = (name: string | null | undefined) => name || "Someone";

// Who saved the notes that came in while the user was editing.
export function conflictText(notes: Pick<Notes, "updated_by">) {
  return `${editorName(notes.updated_by)} saved these notes while you were editing. Your text is kept below.`;
}

// "1 character", "2,048 characters".
export const charactersText = (count: number) => `${count.toLocaleString()} ${count === 1 ? "character" : "characters"}`;
