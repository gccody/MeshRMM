import assert from "node:assert/strict";
import test from "node:test";
import { MAX_NOTES_CHARS, charactersText, conflictText, notesParts, notesProblem } from "../features/notes/model.ts";

test("notes split into text and web links, never HTML", () => {
  assert.deepEqual(notesParts("See https://example.com/a?b=1, then call."), [
    { kind: "text", text: "See " },
    { kind: "link", text: "https://example.com/a?b=1" },
    { kind: "text", text: ", then call." },
  ]);
  assert.deepEqual(notesParts("(wiki: http://wiki/x_(y))"), [
    { kind: "text", text: "(wiki: " },
    { kind: "link", text: "http://wiki/x_(y)" },
    { kind: "text", text: ")" },
  ]);
  assert.deepEqual(notesParts("<b>bold</b> javascript:alert(1) ftp://x"), [{ kind: "text", text: "<b>bold</b> javascript:alert(1) ftp://x" }]);
  assert.deepEqual(notesParts(""), []);
  assert.deepEqual(notesParts("a\n  https://x.test\nb").map((part) => part.kind), ["text", "link", "text"]);
});

test("notes have a length limit counted in characters", () => {
  assert.equal(notesProblem("x".repeat(MAX_NOTES_CHARS)), null);
  assert.match(notesProblem("x".repeat(MAX_NOTES_CHARS + 1)), /at most 20,000/);
  assert.equal(notesProblem("😀".repeat(MAX_NOTES_CHARS)), null);
});

test("a conflict names who saved meanwhile", () => {
  assert.match(conflictText({ updated_by: "Bob" }), /^Bob saved these notes while you were editing/);
  assert.match(conflictText({ updated_by: null }), /^Someone saved/);
  assert.equal(charactersText(1), "1 character");
  assert.equal(charactersText(2048), "2,048 characters");
});
