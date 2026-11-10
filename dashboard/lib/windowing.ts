// The rows of a long list of fixed-height rows to draw for a scroll
// position: those on screen and `overscan` either side.
export function windowRows(scrollTop: number, viewport: number, total: number, rowHeight: number, overscan = 10) {
  // Scrolled past the end, as a page may be, draws nothing.
  const first = Math.min(total, Math.max(0, Math.floor(scrollTop / rowHeight) - overscan));
  const last = Math.min(total, Math.ceil((scrollTop + viewport) / rowHeight) + overscan);
  return { first, last: Math.max(first, last) };
}

// The items of a long grid, `columns` to a row, to draw for a scroll
// position: whole rows as windowRows chooses them, and the height of the
// rows not drawn above (`above`) and below them (`below`). `rowHeight` is
// the distance from one row's top to the next's, so a list's own rows are
// a grid of one column.
export function windowGrid(scrollTop: number, viewport: number, total: number, columns: number, rowHeight: number, overscan = 10) {
  const perRow = Math.max(1, Math.floor(columns));
  const rows = Math.ceil(total / perRow);
  const { first, last } = windowRows(scrollTop, viewport, rows, rowHeight, overscan);
  return {
    first: Math.min(total, first * perRow),
    last: Math.min(total, last * perRow),
    above: first * rowHeight,
    below: (rows - last) * rowHeight,
  };
}

// The columns a CSS grid lays out, from its computed `grid-template-columns`
// (one length per column, as in "240px 240px 240px").
export const gridColumns = (template: string) => Math.max(1, template.trim().split(/\s+/).filter((track) => track && track !== "none").length);
