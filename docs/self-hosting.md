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
| 49160–49200 (`turn.relay_port_min`–`max`) | UDP | TURN relays | Agents and viewers |

Behind a reverse proxy the server listens on `127.0.0.1:8080` instead of 443,
and the proxy takes 443. If something else already uses 443 on the machine,
set `http.listen` to another port and forward port 443 of the public address
to it: Let's Encrypt's challenge always arrives on port 443. TURN can't go through the proxy: its ports must reach
the server directly.

Each peer that relays through TURN holds one relay port while its session
lasts, so the default range serves about 20 relayed sessions at once. Widen it
if you need more, and open the same range in your firewall.

## Install with Docker

The image is `ghcr.io/gccody/meshrmm-server:<version>` (and `:latest`) for
`linux/amd64` and `linux/arm64`. It has the server, the website, and the
release's Agent and viewer builds, and runs as an unprivileged user (UID
65532). Keep `/var/lib/meshrmm` on a volume.

```sh
docker volume create meshrmm-data
docker run -d --name meshrmm --restart unless-stopped \
  -p 443:443/tcp -p 3478:3478/udp -p 49160-49200:49160-49200/udp \
  -v meshrmm-data:/var/lib/meshrmm \
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
    ports:
      - "443:443/tcp"
      - "3478:3478/udp"
      - "49160-49200:49160-49200/udp"
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

To check a configuration without starting the server, run the same container
with `check-config` as its command:

```sh
docker run --rm -v meshrmm-data:/var/lib/meshrmm -e ... \
  ghcr.io/gccody/meshrmm-server:latest check-config
```

Run admin commands in the running container, for example
`docker exec meshrmm meshrmm-server admin reset-password you@example.com`.

TURN relays send traffic from the server's public address, which the server
reads from `turn.host`'s DNS record at startup. Docker's port publishing on
Linux keeps peers' addresses, which TURN needs. Docker Desktop on macOS and
Windows doesn't, so use it only for trying the server out, with
`MESHRMM_TURN__ENABLED=false` or with peers that connect directly.

With `--network host`, the unprivileged user can't bind port 443. Run the
container with `--user 0:0`, or set `MESHRMM_HTTP__LISTEN=0.0.0.0:8443` and
forward 443 to it.

## Install from the tarball

Each release has `meshrmm-server-<version>-linux-<x86_64|aarch64>.tar.gz`,
with `SHA256SUMS` beside it. The tarball's `install.sh` installs:

- `/usr/bin/meshrmm-server`, a static binary that runs on any Linux
  distribution;
- the Agent and viewer builds in `/usr/share/meshrmm/downloads`;
- a systemd unit, `meshrmm-server.service`, which runs the server as the
  `meshrmm` user with most of the file system read-only;
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
  default), so toolbox uploads fit.

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
  toolbox uploads. Keep `toolbox.max_file_bytes` below that limit.
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

## Database

### SQLite

The default. The server creates `meshrmm.db` in `data_dir` and uses it in WAL
mode. To put it elsewhere:

```toml
[database]
url = "sqlite:///srv/meshrmm/meshrmm.db"
```

### PostgreSQL

Create a database and a user that owns it:

```sh
sudo -u postgres createuser --pwprompt meshrmm
sudo -u postgres createdb --owner meshrmm meshrmm
```

```toml
[database]
url = "postgres://meshrmm:<password>@db.internal/meshrmm"
max_connections = 10
```

Add `?sslmode=require` to the URL if PostgreSQL is on another machine.
`MESHRMM_DATABASE__URL` keeps the password out of the file.

Both backends hold the same data; choose one at install. The server creates
its tables on first start and applies any new migrations when an upgraded
server starts. There's no tool to move an install from one backend to the
other.

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
  because whoever controls them can sign in as anyone.
- **Settings** holds the company's remote-session policy.
- **Devices → Add device** downloads a Windows installer or shows the macOS
  install command. Both enroll the device with this server.

Technicians download the viewer from the links in the website's sidebar.
Agents and viewers update themselves from the server.

### Locked out

These commands work on the server even when nobody can sign in. They use the
same configuration as the server and can run while it runs:

```sh
meshrmm-server admin create-user you@example.com --name "You"   # an Administrator by default
meshrmm-server admin reset-password you@example.com             # also re-enables the account
meshrmm-server admin reset-two-factor you@example.com
```

Each prints a link to open in the browser. With systemd, run them as the
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
   server's user (`meshrmm`, or UID 65532 in Docker) with mode `0700` on the
   directory and `0600` on the key.
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

The server restarts in a few seconds. Agents reconnect by themselves, and
remote sessions that were open resume where the viewer and Agent can.

Agents then update themselves from the server: each checks the server's update
manifest when it starts and every six hours, and installs a newer build that
carries a valid release signature. Viewers check when a session starts. An
Agent or viewer never installs a build without the release signature, whatever
the server offers.

Don't run an older release against a database a newer one has migrated: the
server refuses to start, and `check-config` reports the mismatch. To go back,
restore the backup taken before the upgrade.

## Monitoring and logs

- `GET /healthz` returns `200 {"status":"ok","schema_version":N}` while the
  database answers and has the schema this release expects, and `503`
  otherwise.
- The server logs to standard error: `journalctl -u meshrmm-server` or
  `docker logs meshrmm`. Set `log.format = "json"` for a log collector, and
  `log.level` to a tracing filter such as `meshrmm_server=debug,info` to
  investigate a problem.
- **Audit** in the website lists sign-ins, account and role changes, setting
  changes, enrollments, remote sessions, script runs and file deliveries.
- On `SIGTERM` the server stops taking new connections and gives open
  requests 10 seconds to finish.

## Troubleshooting

- **The browser says the certificate is invalid, or Agents can't connect.**
  In `acme` mode, check the log for `ACME` errors: the name must resolve to
  this server and port 443 must reach it from the internet. In `files` mode,
  check that the file has the full chain. In every mode the certificate's
  name must match `public_url`, and whatever terminates TLS must offer TLS 1.3.
- **Sign-in or saving settings fails with 403.** The page's origin isn't
  `public_url`'s origin. Open the server by exactly that address, and make
  sure a reverse proxy passes `Host` and `Origin` through.
- **Remote sessions connect only on the same network.** TURN isn't reachable:
  check that UDP 3478 and the relay ports are open and forwarded, that
  `check-config` prints the right relay address, and that `turn.host` isn't
  behind an HTTP proxy.
- **The website shows an error about missing downloads, or Agents don't
  update.** `check-config` reports what the downloads directory holds and
  whether the builds are signed.
