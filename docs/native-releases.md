# Automated native releases

MeshRMM publishes its Agent and native viewers from GitHub Actions. After the
one-time secret setup, a normal release consists of changing one value in
`release.json` and pushing the commit to `main`.

## One-time GitHub setup

In the GitHub repository, open **Settings → Secrets and variables → Actions**
and add this repository or `production` environment secret:

- `CLOUDFLARE_API_TOKEN` — a Cloudflare API token created from the **Edit
  Cloudflare Workers** template and restricted to the account and
  `meshrmm.com` zone used by this repository. Never commit this token.

The Cloudflare account ID is already non-secret deployment configuration in the
repository. The workflow passes it to Wrangler automatically.

For production macOS signing, export the Developer ID Application certificate
and add all three secrets below. If they are omitted, CI still produces an
ad-hoc signed development archive and emits a warning:

- `MACOS_CERTIFICATE_P12_BASE64` — the exported `.p12`, base64 encoded as one
  line.
- `MACOS_CERTIFICATE_PASSWORD` — the `.p12` export password.
- `MACOS_CODESIGN_IDENTITY` — the full certificate name, such as
  `Developer ID Application: Example Company (TEAMID)`.

To notarize the macOS viewers, also add all three of these secrets. They must be
configured together:

- `MACOS_NOTARY_APPLE_ID`
- `MACOS_NOTARY_TEAM_ID`
- `MACOS_NOTARY_PASSWORD` — an app-specific Apple ID password.

The workflow uses GitHub's `production` environment. If that environment has
required reviewers, approve the deployment after all native builds pass.

## Publish an update

1. Change only `version` in `release.json` to a higher semantic version.
2. Commit the change and push it to `main`.
3. Watch the **Publish native release** workflow in the repository's Actions
   tab.

For example:

```json
{
  "version": "0.2.2",
  "download_origin": "https://meshrmm.com",
  "viewer_server": "https://api.meshrmm.com"
}
```

CI builds these targets in parallel:

- `agent-windows-x64`
- `client-windows-x64`
- `client-macos-arm64`

GitHub CI publishes only the current Apple Silicon macOS viewer. The local
macOS wrapper retains its x64 build path for development or manual legacy
builds, but x64 is not included in the automated update manifest.

Windows and Apple Silicon use separate persistent Rust build caches. CI restores
the Cargo registry, dependency artifacts, and unchanged workspace crates before
building, and saves useful compiler output even when a later packaging or
deployment step fails. The first build on each platform is still a cold build;
subsequent releases reuse the cache until the Rust toolchain or Cargo dependency
configuration changes.

The deploy job waits for every build, assembles
`dashboard/public/downloads/update-manifest.json`, uploads a recoverable copy of
the complete release to the workflow run, and deploys the dashboard to
Cloudflare. No server deployment or D1 migration is involved.

Agents discover the update at service startup or within six hours. Viewers
discover it the next time a dashboard remote session launches.

## Retry or recover

If a transient failure occurs, open the failed workflow and use **Re-run all
jobs**. The `workflow_dispatch` trigger can also republish the current version
without another version bump.

Each run retains the platform artifacts for 14 days and the assembled release
for 30 days. The deploy job is the only job that writes to Cloudflare, so a
failed platform build cannot publish a partial manifest.
