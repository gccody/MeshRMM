// The rows of a long list of fixed-height rows to draw for a scroll
// position: those on screen and `overscan` either side.
export function windowRows(scrollTop: number, viewport: number, total: number, rowHeight: number, overscan = 10) {
  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const last = Math.min(total, Math.ceil((scrollTop + viewport) / rowHeight) + overscan);
  return { first, last: Math.max(first, last) };
}
