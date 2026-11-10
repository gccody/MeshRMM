// A list the server sends a page at a time for one set of filters, and how
// answers that arrive in any order join it. No imports, so node:test can
// load this module directly.

// The pages loaded so far for the filters `key`, and how many items match
// them. Requests are numbered as they're sent: `listTicket` is the one
// whose first page the list starts with, `countsTicket` the one whose
// `matching` it shows.
export type PagedList<T> = {
  key: string;
  items: T[];
  matching: number;
  listTicket: number;
  countsTicket: number;
};

// The server's answer to request `ticket` for the filters `key`: the page
// from `offset`, and how many match.
export type PageAnswer<T> = {
  key: string;
  ticket: number;
  offset: number;
  items: readonly T[];
  matching: number;
};

// The list after an answer, while the page wants the filters `wanted`. A
// newer request's answer always wins: an older one changes nothing it
// already shows. A page for other filters than those wanted is dropped, and
// a further page joins only the list it was asked for. The items may have
// moved between pages: one already loaded (by `id`) stays where it is.
export function applyPage<T, L extends PagedList<T>>(
  list: L | null,
  answer: PageAnswer<T>,
  wanted: string,
  id: (item: T) => string,
): L | PagedList<T> | null {
  const { ticket } = answer;
  if (answer.key !== wanted) return list;
  if (answer.offset === 0) {
    if (list && list.key === answer.key && ticket < list.listTicket) return list;
    const fresh: PagedList<T> = { key: answer.key, items: [...answer.items], matching: answer.matching, listTicket: ticket, countsTicket: ticket };
    if (!list || list.countsTicket < ticket) return fresh;
    return { ...fresh, countsTicket: list.countsTicket, matching: list.key === answer.key ? list.matching : answer.matching };
  }
  if (!list || list.key !== answer.key || ticket < list.listTicket) return list;
  const loaded = new Set(list.items.map(id));
  const items = [...list.items, ...answer.items.filter((item) => !loaded.has(id(item)))];
  return ticket > list.countsTicket
    ? { ...list, items, matching: answer.matching, countsTicket: ticket }
    : { ...list, items };
}

// How many more items match than are loaded.
export const remainingItems = (list: PagedList<unknown> | null) => (list ? Math.max(0, list.matching - list.items.length) : 0);
