import assert from "node:assert/strict";
import test from "node:test";
import { gridColumns, windowGrid, windowRows } from "../lib/windowing.ts";

// A desktop wall: tiles 290px apart, a 900px window, one row either side.
const ROW = 290;
const VIEWPORT = 900;

// The rows of a grid that show any part of themselves in the view.
const shownRows = (scrollTop, viewport, total, columns, rowHeight) => {
  const rows = Math.ceil(total / columns);
  const first = Math.max(0, Math.floor(scrollTop / rowHeight));
  const last = Math.min(rows, Math.ceil((scrollTop + viewport) / rowHeight));
  return { first, last: Math.max(first, last) };
};

// Whatever the scroll position, the grid draws every item on screen, no
// more than `overscan` rows either side, and space for the rest that adds
// up to the whole grid's height.
const assertWindow = (scrollTop, viewport, total, columns, rowHeight, overscan) => {
  const drawn = windowGrid(scrollTop, viewport, total, columns, rowHeight, overscan);
  const rows = Math.ceil(total / columns);
  const shown = shownRows(scrollTop, viewport, total, columns, rowHeight);
  const context = JSON.stringify({ scrollTop, viewport, total, columns, rowHeight, overscan, drawn });
  assert.ok(drawn.first <= shown.first * columns, context);
  assert.ok(drawn.last >= Math.min(total, shown.last * columns), context);
  assert.ok(drawn.last - drawn.first <= (shown.last - shown.first + 2 * overscan) * columns, context);
  // The space stands in for whole rows, and the rows drawn fill the rest.
  const firstRow = drawn.above / rowHeight;
  const lastRow = rows - drawn.below / rowHeight;
  assert.ok(Number.isInteger(firstRow) && Number.isInteger(lastRow) && firstRow <= lastRow, context);
  assert.equal(drawn.first, Math.min(total, firstRow * columns), context);
  assert.equal(drawn.last, Math.min(total, lastRow * columns), context);
};

test("a grid draws the rows on screen and those either side", () => {
  assert.deepEqual(windowGrid(0, VIEWPORT, 1000, 4, ROW, 1), { first: 0, last: 20, above: 0, below: 245 * ROW });
  // Scrolled to row 10, which shows rows 10 to 13: rows 9 to 14.
  assert.deepEqual(windowGrid(ROW * 10, VIEWPORT, 1000, 4, ROW, 1), { first: 36, last: 60, above: 9 * ROW, below: 235 * ROW });
  assert.deepEqual(windowGrid(ROW * 10 + 100, VIEWPORT, 1000, 4, ROW, 1), { first: 36, last: 60, above: 9 * ROW, below: 235 * ROW });
  for (const scrollTop of [0, 1, 145, ROW, ROW * 3 - 1, ROW * 100.5, ROW * 248, ROW * 250]) assertWindow(scrollTop, VIEWPORT, 1000, 4, ROW, 1);
});

test("a grid with no items, or fewer than a row, draws them all with no space", () => {
  assert.deepEqual(windowGrid(0, VIEWPORT, 0, 4, ROW, 1), { first: 0, last: 0, above: 0, below: 0 });
  assert.deepEqual(windowGrid(500, VIEWPORT, 0, 4, ROW, 1), { first: 0, last: 0, above: 0, below: 0 });
  assert.deepEqual(windowGrid(0, VIEWPORT, 3, 6, ROW, 1), { first: 0, last: 3, above: 0, below: 0 });
});

test("a grid scrolled past its end draws nothing and keeps its height", () => {
  assert.deepEqual(windowGrid(ROW * 50, VIEWPORT, 10, 4, ROW, 1), { first: 10, last: 10, above: 3 * ROW, below: 0 });
  assert.deepEqual(windowRows(33 * 900, 600, 100, 33), { first: 100, last: 100 });
});

test("a grid's last row may be part full", () => {
  // Ten items four to a row: rows of 4, 4 and 2.
  assert.deepEqual(windowGrid(0, ROW, 10, 4, ROW, 0), { first: 0, last: 4, above: 0, below: 2 * ROW });
  assert.deepEqual(windowGrid(ROW * 2, ROW, 10, 4, ROW, 0), { first: 8, last: 10, above: 2 * ROW, below: 0 });
  assert.deepEqual(windowGrid(ROW * 2, VIEWPORT, 10, 4, ROW, 1), { first: 4, last: 10, above: ROW, below: 0 });
  for (const scrollTop of [0, ROW, ROW * 2, ROW * 5]) assertWindow(scrollTop, VIEWPORT, 10, 4, ROW, 1);
});

test("a grid draws the same place's items when its columns change", () => {
  // A narrower window: the same scroll position, fewer columns, more rows.
  const wide = windowGrid(ROW * 40, VIEWPORT, 5000, 6, ROW, 1);
  const narrow = windowGrid(ROW * 40, VIEWPORT, 5000, 4, ROW, 1);
  assert.deepEqual(wide, { first: 39 * 6, last: 45 * 6, above: 39 * ROW, below: (Math.ceil(5000 / 6) - 45) * ROW });
  assert.deepEqual(narrow, { first: 39 * 4, last: 45 * 4, above: 39 * ROW, below: (5000 / 4 - 45) * ROW });
  for (const columns of [1, 2, 3, 4, 5, 6, 7]) {
    for (const scrollTop of [0, 777, ROW * 300, ROW * 5000]) assertWindow(scrollTop, VIEWPORT, 5000, columns, ROW, 1);
  }
});

test("a grid of one column is a list's rows", () => {
  for (const scrollTop of [0, 33, 33 * 1000, 33 * 49_990]) {
    const rows = windowRows(scrollTop, 33 * 20, 50_000, 33);
    const grid = windowGrid(scrollTop, 33 * 20, 50_000, 1, 33);
    assert.deepEqual({ first: grid.first, last: grid.last }, rows);
    assert.equal(grid.above, rows.first * 33);
    assert.equal(grid.below, (50_000 - rows.last) * 33);
  }
  // A grid not yet measured counts as one column.
  assert.deepEqual(windowGrid(0, 600, 5, 0, 300, 1), windowGrid(0, 600, 5, 1, 300, 1));
});

test("a hundred thousand devices draw a few rows wherever the wall is scrolled", () => {
  const total = 100_000;
  const columns = 6;
  const rows = Math.ceil(total / columns);
  assert.deepEqual(windowGrid(0, VIEWPORT, total, columns, ROW, 1), { first: 0, last: 30, above: 0, below: (rows - 5) * ROW });
  const middle = windowGrid(ROW * 8000, VIEWPORT, total, columns, ROW, 1);
  assert.deepEqual(middle, { first: 7999 * columns, last: 8005 * columns, above: 7999 * ROW, below: (rows - 8005) * ROW });
  const end = windowGrid(rows * ROW - VIEWPORT, VIEWPORT, total, columns, ROW, 1);
  assert.equal(end.last, total);
  assert.equal(end.below, 0);
  assert.ok(end.last - end.first <= 6 * columns);
  for (const scrollTop of [0, 12_345, ROW * 9000.25, rows * ROW - VIEWPORT]) assertWindow(scrollTop, VIEWPORT, total, columns, ROW, 1);
});

test("a grid's columns come from its computed tracks", () => {
  assert.equal(gridColumns("240px 240px 240px"), 3);
  assert.equal(gridColumns(" 150.5px  150.5px "), 2);
  assert.equal(gridColumns("1240px"), 1);
  assert.equal(gridColumns("none"), 1);
  assert.equal(gridColumns(""), 1);
});
