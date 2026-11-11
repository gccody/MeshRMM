# Running a MeshRMM server

A MeshRMM server is one program, `meshrmm-server`, that serves everything a
company needs: the website, the API, the Agents' and viewers' connections,
STUN/TURN for remote sessions, and the Agent and viewer downloads and updates.
It runs on Linux (x86_64 or arm64) and calls no hosted service. One server
serves one company.

This guide covers installing it with Docker or from the release tarball,
choosing how it gets its TLS certificate, opening its ports, backing it up,
and upgrading it.

## What you need

- A Linux machine or VM. One or two CPU cores and 1 GB of memory are enough
  for a few hundred devices. Remote sessions' video goes directly between the
  viewer and the Agent unless it has to be relayed through TURN, which uses
  the server's bandwidth.
- A DNS name for the server, for example `rmm.example.com`, with an **A
  record** to the server's public IPv4 address. Agents and viewers use TURN
  over IPv4 only.
- A database: SQLite (a file the server creates, the default) or
  PostgreSQL 14 or newer. SQLite is enough for one server; choose PostgreSQL
  if you already run and back it up.
- A certificate. The server can get one from Let's Encrypt itself, use files
  you provide, or sit behind a reverse proxy that handles HTTPS. See
  [TLS](#tls). Agents and viewers require **TLS 1.3**.

## Ports

| Port | Protocol | Used for | Open to |
|---|---|---|---|
| 443 (`http.listen`) | TCP | The website, API, WebSockets, downloads, and the Let's Encrypt challenge | Users, Agents and viewers |
| 3478 (`turn.listen`) | UDP | STUN and TURN | Agents and viewers |
| 23000–24023 (`turn.relay_port_min`–`max`) | UDP | TURN relays | Agents and viewers |

Behind a reverse proxy the server listens on `127.0.0.1:8080` instead of 443,
and the proxy takes 443. If something else already uses 443 on the machine,
set `http.listen` to another port and forward port 443 of the public address
to it: Let's Encrypt's challenge always arrives on port 443. TURN can't go through the proxy: its ports must reach
the server directly.

With `ufw`, for example:

```sh
sudo ufw allow 443/tcp
sudo ufw allow 3478/udp
sudo ufw allow 23000:24023/udp
```

Every remote session holds two relay ports, relayed or not, so the default
range of 1,024 ports serves 512 sessions at once. See [sizing the relay
ports](#sizing-the-relay-ports) to change it, and open the same range in your
firewall.

## Install with Docker

The image is `ghcr.io/gccody/meshrmm-server:<version>` (and `:latest`) for
`linux/amd64` and `linux/arm64`. It has the server, the website, and the
release's Agent and viewer builds, and runs as an unprivileged user (UID
65532). Keep `/var/lib/meshrmm` on a volume. The examples use the host's
network, so the server's ports, including the 1,024 relay ports, are the
host's; see below for why, and for publishing ports instead.

```sh
docker volume create meshrmm-data
docker run -d --name meshrmm --restart unless-stopped \
  --network host --user 0:0 \
  -v meshrmm-data:/var/lib/meshrmm \
  --ulimit nofile=1048576:1048576 \
  -e MESHRMM_PUBLIC_URL=https://rmm.example.com \
  -e MESHRMM_TLS__MODE=acme \
  -e MESHRMM_TLS__CONTACT_EMAIL=admin@example.com \
  ghcr.io/gccody/meshrmm-server:latest
docker logs -f meshrmm   # shows the first-run setup link
```

Or with Docker Compose:

```yaml
services:
  meshrmm:
    image: ghcr.io/gccody/meshrmm-server:latest
    restart: unless-stopped
    network_mode: host
    user: "0:0"
    ulimits:
      nofile: 1048576
    volumes:
      - meshrmm-data:/var/lib/meshrmm
      # Optional: a configuration file instead of environment variables.
      # - ./server.toml:/etc/meshrmm/server.toml:ro
    environment:
      MESHRMM_PUBLIC_URL: https://rmm.example.com
      MESHRMM_TLS__MODE: acme
      MESHRMM_TLS__CONTACT_EMAIL: admin@example.com
volumes:
  meshrmm-data:
```

The `nofile` limit lets the server hold a socket for each Agent; Docker's
default lets it open only 1,024 files. See [connections](#connections).

To check a configuration without starting the server, run the same container
with `check-config` as its command:

```sh
docker run --rm -v meshrmm-data:/var/lib/meshrmm -e ... \
  ghcr.io/gccody/meshrmm-server:latest check-config
```

Run admin commands in the running container, for example
`docker exec meshrmm meshrmm-server admin reset-password you@example.com`.

TURN relays send traffic from the server's public address, which the server
reads from `turn.host`'s DNS record at startup. Both host networking and
Docker's port publishing on Linux keep peers' addresses, which TURN needs.
Docker Desktop on macOS and Windows doesn't, so use it only for trying the
server out, with `MESHRMM_TURN__ENABLED=false` or with peers that connect
directly.

The examples above use host networking because of the relay ports: Docker
publishes a range port by port, and with its default `userland-proxy` it
starts a `docker-proxy` process for each, so publishing 1,024 ports is slow to
start and takes a lot of memory. With `--network host`, the unprivileged user
can't bind port 443, so the examples run the container as root
(`--user 0:0`). To keep the unprivileged user, set
`MESHRMM_HTTP__LISTEN=0.0.0.0:8443` instead and forward port 443 to it.

Or publish the ports instead of using host networking. Set
`"userland-proxy": false` in `/etc/docker/daemon.json` first, or publish fewer
relay ports and set `turn.relay_port_min` and `turn.relay_port_max` to match
(see [sizing the relay ports](#sizing-the-relay-ports)):

```sh
docker run -d --name meshrmm --restart unless-stopped \
  -p 443:443/tcp -p 3478:3478/udp -p 23000-24023:23000-24023/udp \
  -v meshrmm-data:/var/lib/meshrmm \
  ...
```

and in Docker Compose, replace `network_mode` and `user` with:

```yaml
    ports:
      - "443:443/tcp"
      - "3478:3478/udp"
      - "23000-24023:23000-24023/udp"
```

Docker's published ports bypass `ufw`; host networking doesn't.

## Install from the tarball

Each release has `meshrmm-server-<version>-linux-<x86_64|aarch64>.tar.gz`,
with `SHA256SUMS` beside it. The tarball's `install.sh` installs:

- `/usr/bin/meshrmm-server`, a static binary that runs on any Linux
  distribution;
- the Agent and viewer builds in `/usr/share/meshrmm/downloads`;
- `/usr/libexec/meshrmm/rcodesign`, which signs the macOS builds if you
  [set that up](#sign-the-macos-builds);
- a systemd unit, `meshrmm-server.service`, which runs the server as the
  `meshrmm` user with most of the file system read-only, and lets it open
  1,048,576 files (`LimitNOFILE`), enough for a socket per Agent;
- `/etc/meshrmm/server.toml`, copied from the example on a new install.

```sh
sha256sum -c --ignore-missing SHA256SUMS
tar -xzf meshrmm-server-<version>-linux-x86_64.tar.gz
sudo meshrmm-server-<version>-linux-x86_64/install.sh
sudoedit /etc/meshrmm/server.toml
sudo -u meshrmm meshrmm-server check-config
sudo systemctl enable --now meshrmm-server
journalctl -u meshrmm-server -f   # shows the first-run setup link
```

The unit keeps data in `/var/lib/meshrmm`. If you set `data_dir` elsewhere,
add that directory with `sudo systemctl edit meshrmm-server`:

```ini
[Service]
ReadWritePaths=/srv/meshrmm
```

## Configure

The server reads `/etc/meshrmm/server.toml` (another path with
`--config <file>` or `MESHRMM_CONFIG`). `server.example.toml`, installed
beside it, explains every setting. The ones you need:

```toml
public_url = "https://rmm.example.com"

[tls]
mode = "acme"
contact_email = "admin@example.com"
```

Any setting can come from an environment variable instead: `MESHRMM_`, then
the setting's path in capitals with `__` between levels, such as
`MESHRMM_PUBLIC_URL`, `MESHRMM_TLS__MODE` or `MESHRMM_DATABASE__URL`.
Environment variables override the file.

`meshrmm-server check-config` loads the configuration, the certificate files,
the downloads and the database, says what it found, and exits without changing
anything. It also prints the address TURN relays will use.

`public_url` is the address everyone reaches the server at, and must be HTTPS.
The server accepts changes and live connections only from pages on that
origin, and gives it to Agents, viewers and invitation links. Changing it
later means re-enrolling Agents, which keep the address they enrolled with.

## TLS

Choose one `tls.mode`.

### Let's Encrypt (`acme`)

```toml
[tls]
mode = "acme"
contact_email = "admin@example.com"
```

The server gets a certificate for `public_url`'s host and renews it itself,
answering Let's Encrypt's TLS-ALPN-01 challenge on port 443. Port 80 isn't
needed, but port 443 must be reachable from the internet and the name must
resolve to this server. The account and certificates are kept in
`data_dir/acme`.

Until the first certificate arrives, HTTPS connections fail. The log shows
each attempt (`ACME`) and why one failed. To try the setup without Let's
Encrypt's rate limits, use its staging service first, whose certificates
browsers and Agents don't trust:

```toml
directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory"
```

Delete `data_dir/acme` when you switch back, so the server requests a trusted
certificate. `directory_url` can also point at an internal ACME server, and
`domains` lists the names to request if `public_url`'s host isn't the only one.

### Certificate files (`files`)

```toml
[tls]
mode = "files"
cert_path = "/etc/meshrmm/fullchain.pem"
key_path = "/etc/meshrmm/privkey.pem"
```

The server serves the certificate chain and key from PEM files, which suits an
internal CA, certbot, or a Cloudflare origin certificate. It checks the files
every minute and loads them again when either changes, so a renewal needs no
restart. If the new files don't load, it keeps the old certificate and logs a
warning. The files must be readable by the server's user.

Agents and viewers check the certificate against the operating system's
trusted roots. With an internal CA, trust its root on every device and every
technician's computer.

### Behind a reverse proxy (`proxy`)

```toml
[tls]
mode = "proxy"
trusted_proxies = ["127.0.0.1/32", "::1/128"]

[http]
listen = "127.0.0.1:8080"
```

The server serves plain HTTP on `http.listen` (by default `127.0.0.1:8080`)
for a proxy that handles HTTPS. The proxy must:

- offer TLS 1.3;
- pass WebSocket upgrades through and leave long-lived connections open;
- pass the `Host` and `Origin` headers through unchanged;
- set `X-Forwarded-For`, which the server believes only from the addresses in
  `trusted_proxies`, for rate limits and the audit log;
- allow request bodies as large as `toolbox.max_file_bytes` (95 MiB by
  default), so toolbox uploads fit, and at least 64 MiB, the pieces the
  [file browser](files.md#uploads) uploads in;
- stream request and response bodies rather than buffer them, so large
  downloads and uploads flow through as they arrive.

The server leaves HSTS to the proxy in this mode.

Caddy does all of this by default:

```
rmm.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

nginx:

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name rmm.example.com;
    ssl_certificate     /etc/ssl/rmm.example.com/fullchain.pem;
    ssl_certificate_key /etc/ssl/rmm.example.com/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    client_max_body_size 100m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
        proxy_request_buffering off;
        proxy_buffering off;
    }
}

# In the http block:
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}
```

### Cloudflare in front

You can put the server's name behind Cloudflare's proxy (the orange cloud),
with a Cloudflare origin certificate in `files` mode or with `proxy` mode
behind a tunnel. Then:

- TURN can't go through Cloudflare. Add a **DNS-only** record for the server,
  such as `turn.rmm.example.com`, and set `turn.host` to it.
- Set the zone's minimum TLS version to 1.2 or 1.3 and keep TLS 1.3 on;
  Agents and viewers use only TLS 1.3.
- Cloudflare limits request bodies (100 MB on the free plan), which caps
  toolbox uploads. Keep `toolbox.max_file_bytes` below that limit. The file
  browser uploads in 64 MiB pieces, so its uploads fit.
- A Cloudflare origin certificate is trusted only by Cloudflare. If you later
  turn the proxy off, Agents can't connect until the server has a publicly
  trusted certificate.
- Cloudflare closes idle WebSockets after about 100 seconds. Agents, viewers
  and the website send heartbeats more often than that.

## TURN

When a viewer and an Agent can't reach each other directly, for example both
behind strict NAT, the built-in TURN server relays their traffic. Peers get
credentials for it with each session, which work only while the session
lasts. It needs:

- `turn.host`: the name or IPv4 address peers reach it at. It defaults to
  `public_url`'s host, which must then have an A record and must not be
  behind an HTTP proxy.
- `turn.public_ip`: the address relayed traffic comes from. It defaults to
  the address `turn.host` resolves to at startup. Set it when that name
  resolves to a private address, as with split-horizon DNS.
- UDP 3478 and the relay ports open from the internet.

TURN runs over UDP only. Networks that block outbound UDP can't relay, and
sessions from them need a direct path. Set `turn.blocked_peers` to keep relays
away from address ranges that no Agent or viewer uses, such as the server's
own internal network. Loopback, link-local, multicast and broadcast addresses
are always refused. Set `turn.enabled = false` to turn TURN off.

### Sizing the relay ports

Both peers of a remote session, the Agent and the viewer, open a relay on the
server while they gather ICE candidates, before anyone knows whether the
session will need one, and each keeps its relay port until it closes: when the
session ends, or, for a peer that vanished without closing, when its relay
expires up to 10 minutes later. So every live session holds two relay ports
whether or not its traffic goes through the relay, and the range serves half
as many sessions as it has ports. The default, `turn.relay_port_min = 23000`
to `turn.relay_port_max = 24023`, is 1,024 ports for 512 sessions at once.

To size it, double the most remote sessions you expect at once and add some
room. For up to 1,000 sessions, for example, 2,500 ports:

```toml
[turn]
relay_port_min = 23000
relay_port_max = 25499
```

Only relayed sessions use the server's bandwidth. Keep the range clear of
`turn.listen`'s port (the server refuses to start otherwise) and of the ports
other programs on the machine use. The default sits below Linux's ephemeral
ports (32768–60999), which outgoing connections take their ports from; keep a
wider range below them too if you can. Open the new range in your firewall,
and publish it if Docker doesn't use the host's network.

At startup the log says how many sessions the range serves (`serving STUN and
TURN; the relay ports serve 512 remote sessions at once`), and so does
`check-config`. When the range is full, new sessions' peers get no relay;
they still connect if they can reach each other directly. The server logs
`every TURN relay port is in use` with the range and, in `refused`, how many
relays it refused since the last such line, at most once a minute.

## Database

### SQLite

The default. The server creates `meshrmm.db` in `data_dir` and uses it in WAL
mode. To put it elsewhere:

```toml
[database]
url = "sqlite:///srv/meshrmm/meshrmm.db"
```

SQLite lets one connection write at a time, so the server writes through one
connection and reads through a pool of read-only ones. Writes wait their turn
in the server; reads don't wait for writes.
`max_connections` sets the number of readers, by default twice the CPUs, at
least 4 and at most 32. Each connection maps up to 256 MiB of the file, which
the operating system shares between them, and caches up to 8 MiB of pages
(64 MiB for the writer).

### PostgreSQL

Create a database and a user that owns it:

```sh
sudo -u postgres createuser --pwprompt meshrmm
sudo -u postgres createdb --owner meshrmm meshrmm
```

```toml
[database]
url = "postgres://meshrmm:<password>@db.internal/meshrmm"
```

Add `?sslmode=require` to the URL if PostgreSQL is on another machine.
`MESHRMM_DATABASE__URL` keeps the password out of the file.

The server opens up to `max_connections` connections, by default four times
its CPUs, at least 10 and at most 50. Together with anything else that uses
the PostgreSQL server, they must fit its `max_connections` (100 by default).

On either backend, a request that finds every connection it could use busy for
`acquire_timeout_seconds` (5 by default), or a SQLite write that waits for the
writer for `write_timeout_seconds` (10), does nothing and is answered
`503 Service Unavailable` with `Retry-After`. Opening a new connection counts
toward the wait.

Both backends hold the same data; choose one at install. The server creates
its tables on first start and applies any new migrations when an upgraded
server starts. There's no tool to move an install from one backend to the
other.

## Sign the macOS builds

Releases sign the macOS Agent and viewer only ad hoc, so no certificate vouches
for them. They work, with two drawbacks:

- macOS blocks the viewer the first time it's opened after downloading it from
  the website. The technician allows it in **System Settings → Privacy &
  Security → Open Anyway**. The Agent installs from Terminal, which macOS
  doesn't block.
- macOS ties the Agent's Screen Recording, Accessibility and Input Monitoring
  permissions to its signature, so the Mac's user has to grant them again
  after every Agent update.

To avoid both, have the server sign them with your company's Developer ID. You
need an Apple Developer Program membership. Set this up before you enroll
Macs: a Mac installed with an ad-hoc signed Agent or viewer keeps updating to
ad-hoc builds until you reinstall it.

1. Create a **Developer ID Application** certificate in your Apple Developer
   account, and export it with its private key from Keychain Access as a
   `.p12` file.
2. Convert it to the form rcodesign reads, which holds only that certificate
   and uses legacy encryption. On a machine with OpenSSL 3:

   ```sh
   openssl pkcs12 -legacy -in Exported.p12 -clcerts -nodes -out identity.pem
   openssl pkcs12 -export -legacy -in identity.pem -out developer-id.p12
   rm identity.pem
   ```

   Put the export password in a file of its own, such as
   `developer-id.password`.
3. To notarize the builds, which macOS needs before it opens the viewer
   without asking, create an App Store Connect API key (**Users and Access →
   Integrations → App Store Connect API**, with the Developer role), download
   its `.p8` file, and combine it with the key's issuer ID and key ID:

   ```sh
   /usr/libexec/meshrmm/rcodesign encode-app-store-connect-api-key \
     -o notary-api-key.json <issuer-id> <key-id> AuthKey_<key-id>.p8
   ```

4. Copy the files to the server where only it can read them, and configure
   them:

   ```sh
   sudo install -m 0640 -g meshrmm developer-id.p12 developer-id.password \
     notary-api-key.json /etc/meshrmm/
   ```

   ```toml
   [downloads.macos_signing]
   certificate = "/etc/meshrmm/developer-id.p12"
   certificate_password_file = "/etc/meshrmm/developer-id.password"
   notary_api_key = "/etc/meshrmm/notary-api-key.json"
   ```

   With Docker, mount the files read-only and name them with
   `MESHRMM_DOWNLOADS__MACOS_SIGNING__CERTIFICATE`,
   `MESHRMM_DOWNLOADS__MACOS_SIGNING__CERTIFICATE_PASSWORD_FILE` and
   `MESHRMM_DOWNLOADS__MACOS_SIGNING__NOTARY_API_KEY`. The files must be
   readable by UID 65532.
5. Run `meshrmm-server check-config`. It checks that rcodesign opens the
   certificate and that it's a Developer ID Application certificate.

The first time the server starts a release, it signs and notarizes the macOS
builds in the background, which usually takes a few minutes, and logs when
it's done. Until then the macOS installers answer "still signing" and Macs
wait for their update. Later starts reuse the signed builds in
`<data_dir>/developer-id` until the release or the certificate changes. The
server needs outbound access to Apple: `http://timestamp.apple.com` for the
signature's timestamp, and HTTPS to Apple's notary service to notarize.

A Mac signed with your Developer ID only takes updates signed by the same
team. A renewed certificate from the same team works; a different team's
means reinstalling the Macs and viewers.

## First run

When no account exists yet, the server logs a one-time setup link:

```
WARN no accounts exist yet; open this link to create the first administrator (it changes on every restart) link=https://rmm.example.com/setup#token=...
```

Open it to name the server and create the first administrator. Without the
link nobody can set the server up, so someone who reaches it before you can't
take it over. The link changes each time the server starts, until setup is
done.

Then, in the website:

- **Users** invites technicians. With email set up (**Authentication →
  Email**), invitations and password resets are emailed; without it, you copy
  a one-time link and send it yourself.
- **Roles** defines what each role may do. Administrator holds every
  permission; Technician is an editable default.
- **Authentication** sets the sign-in policy (password rules, required
  two-factor), single sign-on with an OIDC provider, and SCIM provisioning
  tokens. Only administrators can change single sign-on, SCIM and email,
  because whoever controls them can sign in as anyone. The server always
  keeps an enabled administrator who can sign in: one with a password or
  passkey, or one linked to single sign-on while it is on. So while every
  administrator signs in only through single sign-on, it can't be turned
  off or removed; give an administrator a password first (a password reset
  link from **Users** sets one). The **Authentication → API tokens**
  tab turns users' [API tokens](api.md) on or off, limits how long they last
  (365 days unless you change it), and lists everyone's tokens for whoever
  holds `api_tokens.manage`.
- **Settings** holds the company's remote-session policy and how long an
  idle [terminal](terminal.md) stays open.
- **Alerts** holds the alert rules and the email and webhook channels they
  notify. Email channels need email set up. See [alerts](alerts.md).
- **Devices → Add device** downloads a Windows installer or shows the macOS
  or Linux install command, each for one computer within 30 minutes. For a
  rollout through Group Policy, an MDM or a script, **Companies →
  Deployment tokens** makes one installer and one command that enroll any
  number of computers into a company until the token expires, is revoked or
  runs out. A leaked one enrolls computers the attacker controls into that
  company until then; see [deployment tokens](devices.md#deployment-tokens).

Technicians download the viewer from the website's Remote app menu, in the top bar.
Agents and viewers update themselves from the server.

### Locked out

These commands work on the server even when nobody can sign in. They use the
same configuration as the server and can run while it runs:

```sh
meshrmm-server admin create-user you@example.com --name "You"   # an Administrator by default
meshrmm-server admin reset-password you@example.com             # also re-enables the account
meshrmm-server admin reset-two-factor you@example.com
```

Each prints a link to open in the browser. A running server can't be told
of what they change, so it may take up to 30 seconds to notice (an
account's API tokens working again after `reset-password` re-enabled it,
for example); signing in with the link applies it at once, and the sessions
`reset-two-factor` ends stop working at once. With systemd, run them as the
`meshrmm` user (`sudo -u meshrmm meshrmm-server admin ...`); with Docker, use
`docker exec meshrmm meshrmm-server admin ...`.

## Backups

Back up two things, taken at the same time:

1. **The database.**
2. **The data directory** (`data_dir`, `/var/lib/meshrmm` by default). It
   holds:
   - `instance.key`, which encrypts the secrets in the database (two-factor
     secrets, the SSO client secret, the SMTP password) and signs TURN
     credentials. **A database backup is useless without it.** Keep it as
     safe as the database: anyone with both can read those secrets.
   - `toolbox/`: the toolbox library's files.
   - `thumbnails/`: device screen thumbnails, which Agents replace anyway.
   - `acme/`: the Let's Encrypt account and certificate, which the server
     requests again if they're missing.
   - `meshrmm.db` (and its `-wal` and `-shm` files) with SQLite in the
     default place. Back it up as below, not by copying these files while
     the server runs.

The Agent and viewer builds come with the release, so they don't need a
backup.

### SQLite

Copy the database while the server runs with SQLite's backup command:

```sh
sqlite3 /var/lib/meshrmm/meshrmm.db ".backup '/backup/meshrmm.db'"
```

In Docker the image has no `sqlite3`. Run it from another container that
mounts the same volume:

```sh
docker run --rm -v meshrmm-data:/data:ro -v "$PWD":/backup alpine \
  sh -c 'apk add -q sqlite && sqlite3 "file:/data/meshrmm.db?mode=ro" ".backup /backup/meshrmm.db"'
```

Then copy the rest of the data directory:

```sh
tar -C /var/lib/meshrmm -czf /backup/meshrmm-data.tar.gz \
  --exclude='meshrmm.db*' .
```

In Docker:

```sh
docker run --rm -v meshrmm-data:/data:ro -v "$PWD":/backup alpine \
  tar -C /data -czf /backup/meshrmm-data.tar.gz --exclude='meshrmm.db*' .
```

### PostgreSQL

```sh
pg_dump --format=custom --file=/backup/meshrmm.dump "postgres://meshrmm@db.internal/meshrmm"
tar -C /var/lib/meshrmm -czf /backup/meshrmm-data.tar.gz .
```

### Restore

1. Stop the server.
2. Restore the data directory, including `instance.key`, owned by the
   server's user (`meshrmm`, or in Docker UID 65532 unless the container runs
   as root) with mode `0700` on the directory and `0600` on the key.
3. Restore the database: copy the SQLite file to its path (removing any
   `meshrmm.db-wal` and `meshrmm.db-shm` left there), or for PostgreSQL
   create an empty database and run
   `pg_restore --no-owner --role=meshrmm --dbname=<url> /backup/meshrmm.dump`.
4. Run `meshrmm-server check-config`, then start the server with the same
   release or a newer one.

In Docker, restore into a new volume:

```sh
docker volume create meshrmm-data
docker run --rm -v meshrmm-data:/data -v "$PWD":/backup:ro alpine sh -c '
  tar -C /data -xzf /backup/meshrmm-data.tar.gz &&
  cp /backup/meshrmm.db /data/ &&
  chown -R 65532:65532 /data && chmod 700 /data && chmod 600 /data/instance.key'
```

(Leave out the `cp` line with PostgreSQL.)

Devices enrolled after the backup was taken, and Agents whose credentials
were rotated since, can't sign in with the restored database. Delete their
old entries and enroll them again with a new installer from **Add device**.

## Upgrades

A release updates the server, the website, and the Agent and viewer builds
together. The server applies database migrations when it starts, so there's
nothing to run by hand.

1. Back up the database and data directory.
2. Install the new release:
   - Docker: `docker pull ghcr.io/gccody/meshrmm-server:<version>`, then
     recreate the container with the new image (`docker compose up -d` with
     Compose) and the same volume.
   - Tarball: unpack the new tarball, run its `install.sh`, then
     `sudo systemctl restart meshrmm-server`. The installer leaves
     `server.toml` alone.
3. Check `https://rmm.example.com/healthz` and the log.

The server restarts in a few seconds. Agents reconnect by themselves, each
within about two minutes of the server coming back and spread out at random
so they don't all arrive at once, and remote sessions that were open resume where the viewer and Agent can.

Agents then update themselves from the server: each asks the server for its
update when it starts and every six hours, and installs a newer build that
carries a valid release signature. By default every device is offered the new
release at once. To try it on a few devices first, choose **Some devices
first** under **Settings → Agent updates** before upgrading: the new release
then starts at the first share (10 percent unless changed), and you advance,
pause or resume it there. A change reaches each device at its next check, up
to about six and a half hours later, and pausing doesn't undo installs; see
[staged rollouts](devices.md#staged-rollouts). Viewers check when a session
starts. An
Agent or viewer never installs a build without the release signature, whatever
the server offers, except that a Mac app signed with your Developer ID installs
only builds signed by the same team instead. With macOS signing set up, Macs
update once the server has signed the new release's builds.

Don't run an older release against a database a newer one has migrated: the
server refuses to start, and `check-config` reports the mismatch. To go back,
restore the backup taken before the upgrade.

## Connections

Each Agent keeps one WebSocket open to the server, and every connection,
from Agents, browsers, terminals, file transfers, TURN relays and the
database alike, takes one of the process's open files. Past the limit the
server can accept no more, and logs `could not accept a connection` with
`Too many open files`, at most once every 10 seconds. The systemd unit
and the Docker examples above set the limit to 1,048,576. If it is below
65,536 when the server starts, the server logs a warning naming it.

The server closes connections that stall before they make a request:

- 10 seconds for the TLS handshake;
- 15 seconds for HTTP to start once the connection is open, and 15 seconds
  for a request's headers to arrive. An HTTP/1 connection that sends no new
  request for 15 seconds is closed too, so a reverse proxy that keeps
  connections to the server open between requests should let them go
  sooner;
- HTTP/2 connections are pinged every 30 seconds and closed if an answer
  doesn't come within 20.

Nothing limits how long a request may take once its headers are in, so
WebSockets, the event stream, and large downloads and uploads run as long
as they need. TCP keep-alive probes a connection that has been silent for a
minute, and closes it after three unanswered probes 10 seconds apart, which
finds a peer that vanished without closing its connection.

Agents and viewers that reconnect resume their TLS session instead of
making a full handshake, with tickets whose keys the server keeps in memory
and changes every 6 hours. After a restart each client makes one full
handshake again.

## Monitoring and logs

- `GET /healthz` returns `200 {"status":"ok","schema_version":N}` while the
  database answers and has the schema this release expects, and `503`
  otherwise.
- The server logs to standard error: `journalctl -u meshrmm-server` or
  `docker logs meshrmm`. Set `log.format = "json"` for a log collector, and
  `log.level` to a tracing filter such as `meshrmm_server=debug,info` to
  investigate a problem. If whatever reads standard error falls behind, the
  server drops lines rather than wait for it, and logs how many it dropped
  once it catches up.
- **Audit** in the website lists sign-ins, account and role changes, setting
  changes, enrollments, remote sessions, script runs and file deliveries,
  and names the API token when someone acted with one.
- API tokens, session cookies and Agent credentials travel in headers,
  which the server doesn't log, so they never appear in its logs.
- On `SIGTERM` the server stops taking new connections and gives open
  requests 10 seconds to finish.

## Troubleshooting

- **The browser says the certificate is invalid, or Agents can't connect.**
  In `acme` mode, check the log for `ACME` errors: the name must resolve to
  this server and port 443 must reach it from the internet. In `files` mode,
  check that the file has the full chain. In every mode the certificate's
  name must match `public_url`, and whatever terminates TLS must offer TLS 1.3.
  If the log says `could not accept a connection` with `Too many open
  files`, raise the open-file limit (see [connections](#connections)).
- **Sign-in or saving settings fails with 403.** The page's origin isn't
  `public_url`'s origin. Open the server by exactly that address, and make
  sure a reverse proxy passes `Host` and `Origin` through.
- **Remote sessions connect only on the same network.** TURN isn't reachable:
  check that UDP 3478 and the relay ports are open and forwarded, that
  `check-config` prints the right relay address, and that `turn.host` isn't
  behind an HTTP proxy. If the log says `every TURN relay port is in use`,
  widen the relay ports (see [sizing the relay
  ports](#sizing-the-relay-ports)).
- **The website shows an error about missing downloads, or Agents don't
  update.** `check-config` reports what the downloads directory holds and
  whether the builds are signed.
- **The macOS installer says the server is still signing, or couldn't sign.**
  Signing and notarizing take a few minutes after a new release first starts.
  If it failed, the log says why (search for `Developer ID`); fix it and
  restart the server to try again.
