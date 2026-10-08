import { StrictMode } from "react";
import { renderToString } from "react-dom/server";
import { StaticRouter } from "react-router";
import { App } from "./app";

export { NOT_FOUND_PATH, NOT_FOUND_TITLE, PAGES } from "./pages";

// The page's HTML as the browser first renders it: nobody is known to be
// signed in yet, so workspace pages show their heading over a loading state.
export function render(path: string) {
  return renderToString(
    <StrictMode>
      <StaticRouter location={path}>
        <App />
      </StaticRouter>
    </StrictMode>,
  );
}
