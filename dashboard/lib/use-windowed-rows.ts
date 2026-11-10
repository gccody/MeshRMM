import { type RefObject, useEffect, useLayoutEffect, useRef, useState } from "react";
import { gridColumns, windowGrid } from "./windowing";

type ElementRef = RefObject<HTMLElement | null>;

// Where a long list scrolls, and what stands in for the rows not drawn:
// either a scroller of its own, holding a spacer (`above`), the rows, and
// another spacer (`below`); or the page, with the list's own padding.
export type WindowedList = { scroller: ElementRef; above: ElementRef; below: ElementRef } | { list: ElementRef };

type Options = {
  // Items to a row, for a grid.
  columns?: number;
  // Rows drawn either side of those on screen.
  overscan?: number;
  // Changes when the list starts over, which scrolls its own scroller back
  // to the top. A page keeps its scroll position.
  listKey?: string;
};

// Draws only the rows of a long list that are on screen: items `first` to
// `last`. `rowHeight` is the distance from one row's top to the next's.
// A scroller of the list's own calls `onScroll`; the page is followed
// without it.
export function useWindowedRows(target: WindowedList, total: number, rowHeight: number, { columns = 1, overscan, listKey = "" }: Options = {}) {
  const own = "scroller" in target ? target : null;
  const scrollerRef = own?.scroller ?? null;
  const aboveRef = own?.above ?? null;
  const belowRef = own?.below ?? null;
  const listRef = "list" in target ? target.list : null;
  // Where the list is scrolled to, in the listing it was scrolled in.
  const [scroll, setView] = useState({ key: listKey, top: 0, height: 600 });
  const view = scroll.key === listKey ? scroll : { ...scroll, top: 0 };
  const drawn = windowGrid(view.top, view.height, total, columns, rowHeight, overscan);
  // The latest render's list, for the scroll and resize handlers.
  const layoutRef = useRef<Layout>({ total, rowHeight, columns, overscan, listKey });
  const frameRef = useRef(0);

  useLayoutEffect(() => {
    layoutRef.current = { total, rowHeight, columns, overscan, listKey };
  });

  // A scroller of the list's own changes size with the layout. The page
  // moves the list when the window resizes or what is above it changes.
  useLayoutEffect(() => {
    const node = (scrollerRef ?? listRef)?.current;
    if (!node) return;
    const own = Boolean(scrollerRef);
    const update = () => setView((current) => follow(current, viewOf(node, own), layoutRef.current));
    update();
    const observer = new ResizeObserver(update);
    observer.observe(own ? node : document.body);
    if (own) return () => observer.disconnect();
    const onScroll = () => {
      cancelAnimationFrame(frameRef.current);
      frameRef.current = requestAnimationFrame(update);
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll);
    return () => {
      observer.disconnect();
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
    };
  }, [scrollerRef, listRef]);

  useEffect(() => () => cancelAnimationFrame(frameRef.current), []);

  useLayoutEffect(() => {
    scrollerRef?.current?.scrollTo({ top: 0, behavior: "instant" });
  }, [scrollerRef, listKey]);

  // The space of the rows not drawn, set directly: the site's content
  // security policy keeps style attributes out of the markup.
  useLayoutEffect(() => {
    aboveRef?.current?.style.setProperty("height", `${drawn.above}px`);
    belowRef?.current?.style.setProperty("height", `${drawn.below}px`);
    listRef?.current?.style.setProperty("padding-top", `${drawn.above}px`);
    listRef?.current?.style.setProperty("padding-bottom", `${drawn.below}px`);
  }, [aboveRef, belowRef, listRef, drawn.above, drawn.below]);

  const onScroll = () => {
    cancelAnimationFrame(frameRef.current);
    frameRef.current = requestAnimationFrame(() => {
      const node = scrollerRef?.current;
      if (node) setView((current) => follow(current, viewOf(node, true), layoutRef.current));
    });
  };

  return { first: drawn.first, last: drawn.last, onScroll };
}

type Layout = { total: number; rowHeight: number; columns: number; overscan: number | undefined; listKey: string };
type View = { key: string; top: number; height: number };

// How far into the list a scroller of its own, or the page, is scrolled,
// and how much of it shows.
const viewOf = (node: HTMLElement, own: boolean) => own
  ? { top: node.scrollTop, height: node.clientHeight }
  : { top: -node.getBoundingClientRect().top, height: window.innerHeight };

// The view to draw from: scrolling within the rows already drawn changes
// nothing, so a long scroll renders once a row, not once a frame.
function follow(current: View, next: { top: number; height: number }, { total, rowHeight, columns, overscan, listKey }: Layout): View {
  if (current.key !== listKey || current.height !== next.height) return { key: listKey, ...next };
  const now = windowGrid(current.top, current.height, total, columns, rowHeight, overscan);
  const then = windowGrid(next.top, next.height, total, columns, rowHeight, overscan);
  return now.first === then.first && now.last === then.last ? current : { key: listKey, ...next };
}

// The columns of a CSS grid of same-height items, and the distance from
// one row's top to the next's, measured from the grid and its first item
// when it is drawn and whenever it changes size. `estimate` stands in
// until an item is drawn.
export function useGridRows(gridRef: RefObject<HTMLElement | null>, estimate: number) {
  const [measured, setMeasured] = useState({ columns: 1, rowHeight: estimate });

  useLayoutEffect(() => {
    const node = gridRef.current;
    if (!node) return;
    const measure = () => {
      const style = getComputedStyle(node);
      const columns = gridColumns(style.gridTemplateColumns);
      const item = node.firstElementChild?.getBoundingClientRect().height ?? 0;
      setMeasured((current) => {
        const rowHeight = item > 0 ? item + (Number.parseFloat(style.rowGap) || 0) : current.rowHeight;
        return current.columns === columns && current.rowHeight === rowHeight ? current : { columns, rowHeight };
      });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [gridRef]);

  return measured;
}
