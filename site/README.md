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

Copy `index.html`, `styles.css`, `favicon.svg` and `fonts/` to any static host.
The page sets its Content Security Policy in a `<meta>` tag. A host that can
set response headers should also send:

```text
Strict-Transport-Security: max-age=31536000; includeSubDomains
Content-Security-Policy: frame-ancestors 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: strict-origin-when-cross-origin
```

`frame-ancestors` only works as a header, not in the `<meta>` tag.
