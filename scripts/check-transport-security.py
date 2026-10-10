#!/usr/bin/env python3
"""Check the TLS a MeshRMM server offers, from outside and without credentials.

Probes the address Agents, viewers and browsers reach: the server itself in
its `acme` and `files` modes, or the proxy in front of it. The host must offer
TLS 1.3, which is all Agents and viewers use, accept TLS 1.2 only with forward
secrecy and an AEAD cipher, and refuse anything older. Port 80 must either be
closed, as it is on a server without a proxy, or redirect to HTTPS.

Run with Python linked to OpenSSL that can probe TLS 1.0/1.1. Local protocol
limitations, certificate failures, DNS failures, and timeouts fail the check;
they must never be reported as evidence that the server rejects legacy TLS.
"""

import argparse
import concurrent.futures
import http.client
import os
import socket
import ssl
import sys
import warnings
from urllib.parse import urlsplit

VERSIONS = (
    ssl.TLSVersion.TLSv1,
    ssl.TLSVersion.TLSv1_1,
    ssl.TLSVersion.TLSv1_2,
    ssl.TLSVersion.TLSv1_3,
)
REMOTE_REJECTIONS = {
    "TLSV1_ALERT_PROTOCOL_VERSION",
    "SSLV3_ALERT_HANDSHAKE_FAILURE",
    "TLSV1_ALERT_INSUFFICIENT_SECURITY",
}


# Explicit negative probes: a preferred modern cipher does not prove that weaker
# alternatives are disabled. These cover CBC and static-RSA AEAD.
FORBIDDEN_CIPHERS = (
    "ECDHE-RSA-AES128-SHA",
    "ECDHE-RSA-AES256-SHA",
    "ECDHE-ECDSA-AES128-SHA",
    "ECDHE-ECDSA-AES256-SHA",
    "AES128-GCM-SHA256",
    "AES256-GCM-SHA384",
    "AES128-SHA",
    "AES256-SHA",
)


def probe(host, version, timeout, minimum, cipher=None, port=443, ca_file=None):
    legacy = version < minimum or cipher is not None
    address = host if port == 443 else f"{host}:{port}"
    label = f"{address} {version.name}" + (f" {cipher}" if cipher else "")
    try:
        # With a CA file, only that CA is trusted, not the system's roots.
        context = ssl.create_default_context(cafile=ca_file)
        context.minimum_version = context.maximum_version = version
        if legacy:
            # Enable obsolete protocols only in this isolated negative probe.
            # Certificate and hostname verification remain enabled.
            context.set_ciphers((cipher or "DEFAULT") + ":@SECLEVEL=0")
        with (
            socket.create_connection((host, port), timeout=timeout) as raw,
            context.wrap_socket(raw, server_hostname=host) as connection,
        ):
            cipher = connection.cipher()[0]
            negotiated = connection.version()
        if legacy:
            return (
                False,
                f"FAIL {label}: forbidden handshake accepted ({negotiated}, {cipher})",
            )
        if not any(name in cipher for name in ("GCM", "CHACHA20", "CCM")) or (
            version == ssl.TLSVersion.TLSv1_2 and not cipher.startswith("ECDHE-")
        ):
            return (
                False,
                f"FAIL {label}: negotiated cipher without required AEAD/forward secrecy: {cipher}",
            )
        return True, f"PASS {label}: verified certificate, {negotiated}, {cipher}"
    except ssl.SSLError as error:
        # Only TLS 1.3 has to be offered; refusing TLS 1.2 is stricter.
        if version < ssl.TLSVersion.TLSv1_3 and error.reason in REMOTE_REJECTIONS:
            return True, f"PASS {label}: server rejected handshake ({error.reason})"
        return False, f"FAIL {label}: probe could not establish policy: {error}"
    except (OSError, ValueError) as error:
        return False, f"FAIL {label}: probe could not establish policy: {error}"


def probe_plain_http(host, timeout):
    connection = http.client.HTTPConnection(host, 80, timeout=timeout)
    try:
        connection.request("HEAD", "/")
        response = connection.getresponse()
        target = urlsplit(response.getheader("Location", ""))
        if (
            response.status in (301, 302, 307, 308)
            and target.scheme == "https"
            and target.hostname == host.lower()
        ):
            return True, f"PASS {host} HTTP: redirects to HTTPS on the same host"
        return (
            False,
            f"FAIL {host} HTTP: expected HTTPS redirect, got status {response.status}",
        )
    except ConnectionRefusedError:
        # A server without a proxy in front of it listens only for HTTPS.
        return True, f"PASS {host} HTTP: nothing listens on port 80"
    except (OSError, ValueError, http.client.HTTPException) as error:
        return False, f"FAIL {host} HTTP: probe could not establish policy: {error}"
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "hosts", nargs="+", help="DNS hostnames to verify (no URL scheme)"
    )
    parser.add_argument(
        "--timeout", type=float, default=10, help="per-connection timeout in seconds"
    )
    parser.add_argument(
        "--minimum-tls",
        choices=("1.2", "1.3"),
        default="1.2",
        help="oldest TLS version the host may accept (default: 1.2); it must "
        "offer 1.3 either way",
    )
    parser.add_argument(
        "--port", type=int, default=443, help="HTTPS port to probe (default: 443)"
    )
    parser.add_argument(
        "--ca-file",
        help="PEM file of the CA or self-signed certificate to trust instead of "
        "the system's roots, for a development server",
    )
    parser.add_argument(
        "--skip-http",
        action="store_true",
        help="don't probe port 80, when a firewall drops it silently or "
        "something other than the server owns it",
    )
    args = parser.parse_args()
    minimum = (
        ssl.TLSVersion.TLSv1_3 if args.minimum_tls == "1.3" else ssl.TLSVersion.TLSv1_2
    )
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    if not 0 < args.port < 65536:
        parser.error("port must be between 1 and 65535")
    if args.ca_file and not os.path.isfile(args.ca_file):
        parser.error(f"no such CA file: {args.ca_file}")
    if any(not host or any(c in host for c in "/: \t\r\n") for host in args.hosts):
        parser.error("supply DNS hostnames, not URLs")
    # These versions are deliberately exercised to prove their rejection.
    warnings.filterwarnings("ignore", category=DeprecationWarning)
    server = {"port": args.port, "ca_file": args.ca_file}
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        checks = [
            pool.submit(probe, host, version, args.timeout, minimum, **server)
            for host in args.hosts
            for version in VERSIONS
        ]
        checks.extend(
            pool.submit(
                probe,
                host,
                ssl.TLSVersion.TLSv1_2,
                args.timeout,
                minimum,
                cipher,
                **server,
            )
            for host in args.hosts
            for cipher in FORBIDDEN_CIPHERS
        )
        if not args.skip_http:
            checks.extend(
                pool.submit(probe_plain_http, host, args.timeout)
                for host in args.hosts
            )
        results = [check.result() for check in checks]
    for _, message in results:
        print(message)
    return 0 if all(passed for passed, _ in results) else 1


if __name__ == "__main__":
    sys.exit(main())
