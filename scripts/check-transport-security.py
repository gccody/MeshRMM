#!/usr/bin/env python3
"""Verify the public TLS policy and HTTPS redirects without credentials.

Run with Python linked to OpenSSL that can probe TLS 1.0/1.1. Local protocol
limitations, certificate failures, DNS failures, and timeouts fail the check;
they must never be reported as evidence that the server rejects legacy TLS.
"""

import argparse
import concurrent.futures
import http.client
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


def probe(host, version, timeout):
    legacy = version < ssl.TLSVersion.TLSv1_2
    label = f"{host} {version.name}"
    try:
        context = ssl.create_default_context()
        context.minimum_version = context.maximum_version = version
        if legacy:
            # Enable obsolete protocols only in this isolated negative probe.
            # Certificate and hostname verification remain enabled.
            context.set_ciphers("DEFAULT:@SECLEVEL=0")
        with socket.create_connection((host, 443), timeout=timeout) as raw:
            with context.wrap_socket(raw, server_hostname=host) as connection:
                cipher = connection.cipher()[0]
                negotiated = connection.version()
        if legacy:
            return False, f"FAIL {label}: obsolete protocol accepted ({negotiated}, {cipher})"
        if not any(name in cipher for name in ("GCM", "CHACHA20", "CCM")):
            return False, f"FAIL {label}: negotiated non-AEAD cipher {cipher}"
        return True, f"PASS {label}: verified certificate, {negotiated}, {cipher}"
    except ssl.SSLError as error:
        if legacy and error.reason in REMOTE_REJECTIONS:
            return True, f"PASS {label}: server rejected handshake ({error.reason})"
        return False, f"FAIL {label}: probe could not establish policy: {error}"
    except (OSError, ValueError) as error:
        return False, f"FAIL {label}: probe could not establish policy: {error}"


def probe_redirect(host, timeout):
    connection = http.client.HTTPConnection(host, timeout=timeout)
    try:
        connection.request("HEAD", "/")
        response = connection.getresponse()
        target = urlsplit(response.getheader("Location", ""))
        if (
            response.status in (301, 302, 307, 308)
            and target.scheme == "https"
            and target.netloc.lower() == host.lower()
        ):
            return True, f"PASS {host} HTTP: redirects to HTTPS on the same host"
        return False, f"FAIL {host} HTTP: expected HTTPS redirect, got status {response.status}"
    except (OSError, ValueError, http.client.HTTPException) as error:
        return False, f"FAIL {host} HTTP: probe could not establish policy: {error}"
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("hosts", nargs="+", help="DNS hostnames to verify (no URL scheme)")
    parser.add_argument("--timeout", type=float, default=10, help="per-connection timeout in seconds")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    if any(not host or any(c in host for c in "/: \t\r\n") for host in args.hosts):
        parser.error("supply DNS hostnames, not URLs")
    # These versions are deliberately exercised to prove their rejection.
    warnings.filterwarnings("ignore", category=DeprecationWarning)
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        checks = [
            pool.submit(probe, host, version, args.timeout)
            for host in args.hosts for version in VERSIONS
        ]
        checks.extend(pool.submit(probe_redirect, host, args.timeout) for host in args.hosts)
        results = [check.result() for check in checks]
    for _, message in results:
        print(message)
    return 0 if all(passed for passed, _ in results) else 1


if __name__ == "__main__":
    sys.exit(main())
