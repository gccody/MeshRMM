import { type RefObject, useEffect, useLayoutEffect, useRef, useState } from "react";
import { windowRows } from "./windowing";

// Draws only the rows of a long list that are on screen. The scroller holds
// a spacer (`above`), the rows `first` to `last`, and another (`below`),
// which stand in for the rows not drawn. `listKey` changes when the list
// starts over, which scrolls it back to the top. The scroller calls
// `onScroll`.
export function useWindowedRows(
  { scroller, above, below }: { scroller: RefObject<HTMLElement | null>; above: RefObject<HTMLElement | null>; below: RefObject<HTMLElement | null> },
  total: number,
  rowHeight: number,
  listKey = "",
) {
  // Where the list is scrolled to, in the listing it was scrolled in.
  const [scroll, setView] = useState({ key: listKey, top: 0, height: 600 });
  const view = scroll.key === listKey ? scroll : { ...scroll, top: 0 };
  const frame = useRef(0);
  const { first, last } = windowRows(view.top, view.height, total, rowHeight);

  useLayoutEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const observer = new ResizeObserver(() => setView((current) => ({ ...current, height: node.clientHeight })));
    observer.observe(node);
    return () => observer.disconnect();
  }, [scroller]);

  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  useLayoutEffect(() => {
    scroller.current?.scrollTo({ top: 0, behavior: "instant" });
  }, [scroller, listKey]);

  // The space of the rows not drawn, set directly: the site's content
  // security policy keeps style attributes out of the markup.
  useLayoutEffect(() => {
    above.current?.style.setProperty("height", `${first * rowHeight}px`);
    below.current?.style.setProperty("height", `${Math.max(0, total - last) * rowHeight}px`);
  }, [above, below, first, last, total, rowHeight]);

  const onScroll = () => {
    cancelAnimationFrame(frame.current);
    frame.current = requestAnimationFrame(() => {
      const node = scroller.current;
      if (node) setView({ key: listKey, top: node.scrollTop, height: node.clientHeight });
    });
  };

  return { first, last, onScroll };
}
