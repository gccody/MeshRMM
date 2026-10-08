// Where to go after signing in. Only a path on this site is followed, so a
// crafted link can't send someone elsewhere after they sign in.
export function safeNextPath(value: string | null | undefined): string {
  if (!value || !value.startsWith("/") || value.startsWith("//") || value.startsWith("/\\")) return "/";
  try {
    const url = new URL(value, "https://meshrmm.invalid");
    if (url.origin !== "https://meshrmm.invalid") return "/";
    const path = `${url.pathname}${url.search}${url.hash}`;
    return url.pathname === "/login" ? "/" : path;
  } catch {
    return "/";
  }
}

// The sign-in page, returning to `path` afterwards.
export function loginPath(path: string) {
  const next = safeNextPath(path);
  return next === "/" ? "/login" : `/login?${new URLSearchParams({ next })}`;
}

// The one-time token of a setup, invitation or reset link. Links carry it in
// the fragment (`#token=…`), which browsers never send to a server.
export function tokenFromHash(hash: string): string | null {
  return new URLSearchParams(hash.replace(/^#/, "")).get("token") || null;
}
