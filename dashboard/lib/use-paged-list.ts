import { useCallback, useEffect, useRef, useState } from "react";
import { AuthenticationRequired, errorText } from "./http";
import { type PagedList, applyPage, remainingItems } from "./paged-list";

export type Page<T> = { items: readonly T[]; matching: number };

// A list the server sends a page at a time: the first page for `key`, again
// whenever `key` changes or a reload is asked for, and further pages on
// `showMore`. `fetchPage` asks for the page of `key`'s filters from an
// offset and changes with `key`; `id` and `messages` keep their identity.
// The newest request's answer wins, so typing fast never leaves an older
// search's results on screen.
export function usePagedList<T>(
  key: string,
  fetchPage: (offset: number) => Promise<Page<T>>,
  id: (item: T) => string,
  messages: { first: string; more: string },
) {
  const [list, setList] = useState<PagedList<T> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  const tickets = useRef(0);
  const wanted = useRef(key);

  useEffect(() => {
    wanted.current = key;
  }, [key]);

  const ask = useCallback(async (offset: number) => {
    const ticket = ++tickets.current;
    try {
      const page = await fetchPage(offset);
      const current = wanted.current;
      setList((list) => applyPage(list, { key, ticket, offset, ...page }, current, id));
      if (key === current) setError(null);
    } catch (failure) {
      if (key === wanted.current && !(failure instanceof AuthenticationRequired)) {
        setError(errorText(failure, offset ? messages.more : messages.first));
      }
    }
  }, [fetchPage, key, id, messages]);

  useEffect(() => {
    void ask(0);
  }, [ask, attempt]);

  const reload = useCallback(() => setAttempt((count) => count + 1), []);

  const showMore = async () => {
    if (!list) return;
    setLoadingMore(true);
    await ask(list.items.length);
    setLoadingMore(false);
  };

  return {
    list,
    error,
    reload,
    showMore,
    loadingMore,
    remaining: remainingItems(list),
    // The list shown is for other filters until the new first page arrives.
    current: list?.key === key,
  };
}
