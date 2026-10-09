# Releases

A MeshRMM release is one version of the server, the Agent and the viewer,
built and published together by the **Publish release** workflow
(`.github/workflows/native-release-build.yml`). Operators download it from
GitHub Releases or GHCR. No running server contacts either.

## What a release contains

- `meshrmm-server-<version>-linux-x86_64.tar.gz` and `…-linux-aarch64.tar.gz`,
  each holding:
  - `bin/meshrmm-server`: the server, statically linked against musl, with
    the website built in;
  - `share/meshrmm/downloads/`: the Windows Agent and viewer, the macOS Agent
    (universal) and viewer (Apple silicon), and `artifacts.json`, which lists
    each build with its SHA-256 and release signature;
  - `libexec/meshrmm/rcodesign`, which signs the macOS builds with the
    company's Developer ID when its server is set up to (see
    [Code signing](#code-signing)), and its license in
    `share/doc/meshrmm/rcodesign/`;
  - `lib/systemd/system/meshrmm-server.service`,
    `etc/meshrmm/server.example.toml`, and `install.sh`, which installs or
    upgrades all of it.
- `SHA256SUMS` for the tarballs.
- The Docker image `ghcr.io/gccody/meshrmm-server:<version>` (and `:latest`),
  for `linux/amd64` and `linux/arm64`, with the same files.

The server serves each build at `/downloads/release/<file>` exactly as the
release shipped it, and at `/downloads/<file>` as the build to install, which
for macOS is the company's Developer ID build when the server signs them
(`/downloads/developer-id/<file>`). It writes
`/downloads/update-manifest.json` from `artifacts.json` and the signed builds,
with every URL on its own `public_url`. Upgrading the server therefore
upgrades its Agents and viewers: Agents check the manifest at startup and
every six hours, and viewers check it when a session starts. The server
refuses to start if a listed build is missing or doesn't match its SHA-256,
and logs a warning if a build isn't signed with the release key it was built
with.

## Code signing

Releases carry no code signing certificate. The Windows builds aren't
Authenticode signed, and the macOS builds are signed only ad hoc. A Developer
ID signature says who vouches for an app, and anyone can download a release,
so a release signed with the maintainer's Developer ID would let anyone deploy
a MeshRMM Agent under the maintainer's name, as attackers do with other remote
access tools.

Each company signs its own instead: with `downloads.macos_signing` set (see
[self-hosting](self-hosting.md#sign-the-macos-builds)), its server signs and
notarizes the macOS builds with the company's Developer ID when it first
starts a release, using the bundled rcodesign, and keeps them in
`<data_dir>/developer-id`. Until it finishes, the macOS installers answer 503.

## Release signatures

Every Agent and viewer build embeds the release public key from
`release.json`'s `signing_public_key`. Before installing an update, it checks
that the manifest entry carries a valid signature from that key over
`meshrmm-release-v1\n<target>\n<version>\n<sha256>\n` (see `signed_message` in
`crates/self-update`). Each server writes its own manifest, but no server can
sign one, so:

- a viewer can take updates from whichever server its dashboard link names,
  including one an attacker controls, without risk of running the attacker's
  code;
- the signature covers the version, so an old build can't be offered as a new
  one;
- Agents refuse updates that were changed in a server's downloads directory.

Signing a macOS build changes it, so the release signature doesn't cover a
company's Developer ID builds. A macOS Agent or viewer signed with a
Developer ID therefore trusts its team's signature instead: it takes only the
manifest's `developer_id` build, and installs it only if `codesign` confirms
that it's the same app (`com.meshrmm.agent` or `com.meshrmm.remote`) signed
with a Developer ID issued to the same team, and that its bundle version is
the version offered. Only that team can sign such a build, so this protects
against a hostile server as the release signature does. An ad-hoc signed
Mac app keeps taking the release build and checking the release signature,
because anyone can make an ad-hoc signature. It can't move to a Developer ID
build by updating; reinstall it from the server.

The private key lives only in the `MESHRMM_RELEASE_SIGNING_KEY` secret. The
`sign` job, which runs only on `main`, signs each build, so builds from other
branches stay unsigned. To rotate the key, a transition release would have to
be signed with the old key while embedding the new one. The workflow doesn't
support that yet.

## One-time GitHub setup

In **Settings → Environments → production**:

- Add the secret `MESHRMM_RELEASE_SIGNING_KEY`: the PKCS #8 PEM private key
  whose public key is in `release.json`.
- Under **Deployment branches and tags**, allow only `main`.

The workflow needs no code signing certificate.

The workflow pushes the Docker image with the run's own token. After the first
push, make the `meshrmm-server` package public under the repository's
**Packages** settings.

## Publish a release

1. Change `version` in `release.json` to a higher semantic version.
2. Open a pull request into `main` and squash-merge it after the checks pass.
3. Watch the **Publish release** workflow in the Actions tab.

The workflow:

1. runs CI and checks that the version increased and isn't published yet;
2. builds the Windows Agent and viewer, the ad-hoc signed macOS Agent and
   viewer, and the server for x86_64 and aarch64 Linux, in parallel;
3. signs the Agent and viewer builds and writes `artifacts.json`;
4. packs both tarballs with rcodesign (a pinned release, checked against its
   SHA-256), installs the x86_64 one on the runner and runs it under systemd,
   and runs the Docker image, checking that each serves every build;
5. pushes the Docker image and creates the `v<version>` GitHub release.

A run on another branch (**Run workflow** with that branch selected) does
steps 1, 2 and 4 with unsigned builds and publishes nothing, which tests a
release before merging.

## Retry or recover

If a transient failure occurs, open the failed run and use **Re-run failed
jobs**. A manual run on `main` publishes the current version if no release
has that version yet and it isn't older than the latest release. Each run keeps
the platform builds for 14 days and the packed release for 30 days.

## Local and development builds

`scripts/build-agent.ps1`, `scripts/build-remote.ps1`,
`scripts/build-agent-macos.sh` and `scripts/build-remote-macos.sh` put their
builds in `dist/downloads/` and update `dist/downloads/artifacts.json`. Point
a development server's `downloads.dir` at that directory to serve them.

Without a signing key the builds are unsigned. The server still serves them
to install, but installed Agents and viewers won't update to them. To test
updates end to end, make a development key and build with it on both sides:

```sh
node scripts/release-artifacts.mjs generate-key ~/meshrmm-dev-key.pem   # prints the public key
export MESHRMM_RELEASE_PUBLIC_KEY=<printed public key>   # builds embed this key
export MESHRMM_RELEASE_SIGNING_KEY="$(cat ~/meshrmm-dev-key.pem)"   # builds are signed with it
```

The Agents, viewers and server built this way trust only that key, so they
won't update to real releases.

`scripts/package-server.sh` packs a tarball from a server binary and a
downloads directory. `scripts/test-server-package.sh` installs and runs one,
which changes the machine it runs on, so it's meant for CI.
