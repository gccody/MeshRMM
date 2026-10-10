"""Policy regressions; no external network access required."""

import contextlib
import http.client
import importlib.util
import io
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

spec = importlib.util.spec_from_file_location(
    "transport_security", Path(__file__).parents[1] / "check-transport-security.py"
)
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)

TLS12 = ssl.TLSVersion.TLSv1_2
TLS13 = ssl.TLSVersion.TLSv1_3


class TransportPolicyTests(unittest.TestCase):
    def negotiated(self, selected_cipher, version=TLS12, minimum=TLS12, **kwargs):
        context = MagicMock()
        connection = context.wrap_socket.return_value.__enter__.return_value
        connection.cipher.return_value = (selected_cipher, version.name, 128)
        connection.version.return_value = version.name
        with (
            patch.object(policy.ssl, "create_default_context", return_value=context),
            patch.object(policy.socket, "create_connection"),
        ):
            return policy.probe("test.invalid", version, 1, minimum, **kwargs)[0]

    def test_tls12_rejected_when_tls13_is_the_minimum(self):
        self.assertFalse(self.negotiated("ECDHE-RSA-AES128-GCM-SHA256", minimum=TLS13))

    def test_tls13_accepted(self):
        for minimum in (TLS12, TLS13):
            with self.subTest(minimum=minimum.name):
                self.assertTrue(
                    self.negotiated("TLS_AES_256_GCM_SHA384", TLS13, minimum)
                )

    def test_tls12_policy_requires_forward_secrecy_and_aead(self):
        for cipher, accepted in (
            ("AES128-GCM-SHA256", False),
            ("ECDHE-RSA-AES128-SHA", False),
            ("ECDHE-RSA-AES128-GCM-SHA256", True),
        ):
            with self.subTest(cipher=cipher):
                self.assertEqual(self.negotiated(cipher), accepted)

    def test_explicit_weak_cipher_acceptance_fails(self):
        for cipher in policy.FORBIDDEN_CIPHERS:
            with self.subTest(cipher=cipher):
                self.assertFalse(self.negotiated(cipher, cipher=cipher))

    def test_only_remote_rejections_pass_negative_probes(self):
        for reason in (
            *policy.REMOTE_REJECTIONS,
            "NO_CIPHERS_AVAILABLE",
            "CERTIFICATE_VERIFY_FAILED",
        ):
            error = ssl.SSLError(reason)
            error.reason = reason
            with (
                self.subTest(reason=reason),
                patch.object(policy.ssl, "create_default_context", side_effect=error),
            ):
                passed, _ = policy.probe("test.invalid", TLS12, 1, TLS13)
                self.assertEqual(passed, reason in policy.REMOTE_REJECTIONS)

    def test_tls13_must_be_offered_but_tls12_need_not_be(self):
        error = ssl.SSLError("TLSV1_ALERT_PROTOCOL_VERSION")
        error.reason = "TLSV1_ALERT_PROTOCOL_VERSION"
        for version, passes in ((TLS12, True), (TLS13, False)):
            with (
                self.subTest(version=version.name),
                patch.object(policy.ssl, "create_default_context", side_effect=error),
            ):
                self.assertEqual(
                    policy.probe("test.invalid", version, 1, TLS12)[0], passes
                )

    def test_network_failure_never_proves_rejection(self):
        for error in (socket.gaierror("DNS failed"), TimeoutError("timed out")):
            with patch.object(policy.socket, "create_connection", side_effect=error):
                self.assertFalse(policy.probe("test.invalid", TLS12, 1, TLS13)[0])

    def test_the_chosen_port_and_ca_file_are_probed(self):
        context = MagicMock()
        connection = context.wrap_socket.return_value.__enter__.return_value
        connection.cipher.return_value = ("TLS_AES_256_GCM_SHA384", "TLSv1.3", 256)
        with (
            patch.object(
                policy.ssl, "create_default_context", return_value=context
            ) as trust,
            patch.object(policy.socket, "create_connection") as connect,
        ):
            passed, message = policy.probe(
                "test.invalid", TLS13, 1, TLS12, port=8443, ca_file="ca.pem"
            )
        self.assertTrue(passed)
        self.assertIn("test.invalid:8443 ", message)
        trust.assert_called_once_with(cafile="ca.pem")
        connect.assert_called_once_with(("test.invalid", 8443), timeout=1)


class PlainHttpTests(unittest.TestCase):
    def answered(self, status, location=None):
        connection = MagicMock()
        response = connection.getresponse.return_value
        response.status = status
        response.getheader.return_value = location or ""
        with patch.object(
            policy.http.client, "HTTPConnection", return_value=connection
        ):
            return policy.probe_plain_http("rmm.example.com", 1)[0]

    def test_a_redirect_to_https_on_the_same_host_passes(self):
        self.assertTrue(self.answered(301, "https://rmm.example.com/"))
        self.assertTrue(self.answered(308, "https://RMM.example.com:8443/"))

    def test_anything_else_served_over_http_fails(self):
        self.assertFalse(self.answered(200))
        self.assertFalse(self.answered(301, "http://rmm.example.com/"))
        self.assertFalse(self.answered(301, "https://elsewhere.example.com/"))

    def test_a_closed_port_passes_but_an_unanswered_one_does_not(self):
        for error, passes in (
            (ConnectionRefusedError("refused"), True),
            (TimeoutError("timed out"), False),
            (socket.gaierror("DNS failed"), False),
            (http.client.BadStatusLine("garbage"), False),
        ):
            connection = MagicMock()
            connection.request.side_effect = error
            with (
                self.subTest(error=type(error).__name__),
                patch.object(
                    policy.http.client, "HTTPConnection", return_value=connection
                ),
            ):
                self.assertEqual(
                    policy.probe_plain_http("rmm.example.com", 1)[0], passes
                )


@unittest.skipUnless(shutil.which("openssl"), "needs the openssl command")
class LocalServerTests(unittest.TestCase):
    """Real handshakes with HTTPS servers on this machine."""

    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.directory.cleanup)
        cls.certificate = str(Path(cls.directory.name, "cert.pem"))
        cls.key = str(Path(cls.directory.name, "key.pem"))
        subprocess.run(
            [
                "openssl", "req", "-x509", "-nodes", "-days", "1",
                "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost",
                "-keyout", cls.key, "-out", cls.certificate,
            ],
            check=True,
            capture_output=True,
        )  # fmt: skip

    def serve(self, minimum=TLS12, maximum=TLS13, ciphers="ECDHE+AESGCM"):
        """Starts a server that completes handshakes; returns its port."""
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(self.certificate, self.key)
        context.minimum_version, context.maximum_version = minimum, maximum
        context.set_ciphers(ciphers)
        listener = socket.create_server(("127.0.0.1", 0))
        self.addCleanup(listener.close)

        def accept():
            while True:
                try:
                    raw, _ = listener.accept()
                except OSError:
                    return
                with contextlib.suppress(OSError), raw:
                    raw.settimeout(5)
                    context.wrap_socket(raw, server_side=True).close()

        threading.Thread(target=accept, daemon=True).start()
        return listener.getsockname()[1]

    def check(self, port, *arguments, trusted=True):
        command = ["check", "localhost", "--port", str(port), "--skip-http"]
        if trusted:
            command += ["--ca-file", self.certificate]
        output = io.StringIO()
        with (
            patch.object(policy.sys, "argv", [*command, *arguments]),
            contextlib.redirect_stdout(output),
        ):
            return policy.main(), output.getvalue()

    def test_a_server_like_meshrmms_passes(self):
        status, output = self.check(self.serve())
        self.assertEqual(status, 0, output)
        self.assertNotIn("FAIL ", output)
        self.assertIn("TLSv1_3: verified certificate, TLSv1.3", output)
        self.assertIn("TLSv1_2: verified certificate, TLSv1.2", output)
        self.assertIn("TLSv1: server rejected handshake", output)

    def test_tls12_fails_when_tls13_is_the_minimum(self):
        port = self.serve()
        status, output = self.check(port, "--minimum-tls", "1.3")
        self.assertEqual(status, 1, output)
        self.assertIn("TLSv1_2: forbidden handshake accepted", output)
        self.assertEqual(self.check(self.serve(minimum=TLS13))[0], 0)

    def test_a_server_agents_cannot_reach_fails(self):
        status, output = self.check(self.serve(maximum=TLS12))
        self.assertEqual(status, 1, output)
        self.assertIn("FAIL localhost", output)

    def test_a_weak_cipher_fails(self):
        status, output = self.check(self.serve(ciphers="ECDHE+AESGCM:ECDHE+AES"))
        self.assertEqual(status, 1, output)
        self.assertIn("ECDHE-ECDSA-AES128-SHA: forbidden handshake accepted", output)

    def test_an_untrusted_certificate_fails_every_probe_it_could_pass(self):
        status, output = self.check(self.serve(), trusted=False)
        self.assertEqual(status, 1, output)
        self.assertIn("CERTIFICATE_VERIFY_FAILED", output)
        self.assertNotIn("verified certificate", output)


if __name__ == "__main__":
    unittest.main()
