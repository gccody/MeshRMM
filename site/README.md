# Marketing site

The public page for MeshRMM: plain HTML and CSS, with no build step, no
JavaScript and nothing loaded from other sites. It's separate from the
website the server embeds (`dashboard/`), and no server serves it.

- `index.html`, `styles.css`, `favicon.svg`: the page.
- `fonts/`: the latin subset of Geist and Geist Mono, the faces the website
  uses (SIL Open Font License, in `fonts/OFL.txt`).
- `site.test.mjs`: checks that every link and asset resolves, that in-page
  links have targets, and that the page runs no scripts and loads nothing from
  other sites. CI runs it with `node --test site/*.test.mjs`.

## Preview

```sh
python3 -m http.server 4719 --bind 127.0.0.1 --directory site
```

Then open <http://127.0.0.1:4719/>.

## Publish

`.github/workflows/site.yml` deploys the site to <https://meshrmm.com> whenever
`site/` changes on main, and can be run by hand from the Actions tab. It runs
`wrangler deploy` here, which publishes the files as a Cloudflare Worker that
serves only static assets:

- `wrangler.jsonc`: the Worker, its account, and the `meshrmm.com` and
  `www.meshrmm.com` custom domains.
- `.assetsignore`: the files in this directory that aren't published.
- `_headers`: response headers Cloudflare sends with every file. The page sets
  its Content Security Policy in a `<meta>` tag, but `frame-ancestors` only
  works as a header.

The workflow needs the `CLOUDFLARE_API_TOKEN` repository secret: an API token
made from Cloudflare's "Edit Cloudflare Workers" template, limited to the
account in `wrangler.jsonc` and the `meshrmm.com` zone. Set it with
`gh secret set CLOUDFLARE_API_TOKEN`. To deploy from your own machine
instead, run `npx wrangler login`, then `npx wrangler deploy` in this directory.

On another static host, publish `index.html`, `styles.css`, `favicon.svg` and
`fonts/`, and send the headers in `_headers`.
