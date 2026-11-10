import assert from "node:assert/strict";
import test from "node:test";
import { applyPage, remainingItems } from "../lib/paged-list.ts";

const id = (item) => item.id;
const items = (...ids) => ids.map((value) => ({ id: value }));
const ids = (list) => list.items.map(id);
const answer = (key, ticket, offset, found, matching = 10) => ({ key, ticket, offset, items: items(...found), matching });

test("further pages join the list they were asked for, each item once", () => {
  const first = applyPage(null, answer("a", 1, 0, ["x", "y"], 5), "a", id);
  assert.deepEqual(ids(first), ["x", "y"]);
  assert.equal(first.matching, 5);
  assert.equal(remainingItems(first), 3);
  // The items moved between pages: "y" comes again, and stays where it was.
  const second = applyPage(first, answer("a", 2, 2, ["y", "z"], 6), "a", id);
  assert.deepEqual(ids(second), ["x", "y", "z"]);
  assert.equal(second.matching, 6, "the newer count");
  assert.equal(remainingItems(second), 3);
  assert.equal(remainingItems(null), 0);
  // A further page for other filters doesn't join.
  assert.equal(applyPage(second, answer("b", 3, 3, ["w"]), "a", id), second);
  // New filters start a new list.
  const other = applyPage(second, answer("b", 4, 0, ["w"], 1), "b", id);
  assert.deepEqual(ids(other), ["w"]);
  assert.equal(other.key, "b");
  assert.equal(remainingItems(other), 0);
});

test("a newer request's answer always wins over an older one's", () => {
  // The filters changed while the first answer was on its way: only the
  // answer for the filters wanted now is shown.
  assert.equal(applyPage(null, answer("a", 1, 0, ["x"]), "b", id), null);
  const shown = applyPage(null, answer("b", 2, 0, ["y"]), "b", id);
  assert.equal(applyPage(shown, answer("a", 1, 0, ["x"]), "b", id), shown);
  // Two loads of the same list: the older answer, arriving last, is dropped,
  // and so is a further page asked for before the newer load.
  const newer = applyPage(shown, answer("b", 4, 0, ["z"], 3), "b", id);
  assert.deepEqual(ids(newer), ["z"]);
  assert.equal(applyPage(newer, answer("b", 3, 0, ["y"]), "b", id), newer);
  assert.equal(applyPage(newer, answer("b", 3, 1, ["q"]), "b", id), newer);
  // A further page asked for after it joins, but a count older than the
  // one shown doesn't replace it.
  const later = applyPage({ ...newer, countsTicket: 9, matching: 7 }, answer("b", 5, 1, ["q"], 2), "b", id);
  assert.deepEqual(ids(later), ["z", "q"]);
  assert.equal(later.matching, 7);
  // A first page older than the counts shown keeps them.
  const reloaded = applyPage(later, answer("b", 6, 0, ["q", "z"], 1), "b", id);
  assert.deepEqual(ids(reloaded), ["q", "z"]);
  assert.equal(reloaded.matching, 7);
  assert.equal(reloaded.listTicket, 6);
});
