"""Policy regressions; no external network access required."""
import importlib.util
from pathlib import Path
import socket
import ssl
import unittest
from unittest.mock import MagicMock, patch

spec = importlib.util.spec_from_file_location(
    "transport_security", Path(__file__).parents[1] / "check-transport-security.py")
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class TransportPolicyTests(unittest.TestCase):
    def negotiated(self, selected_cipher, version=ssl.TLSVersion.TLSv1_2, **kwargs):
        context = MagicMock()
        connection = context.wrap_socket.return_value.__enter__.return_value
        connection.cipher.return_value = (selected_cipher, version.name, 128)
        connection.version.return_value = version.name
        with patch.object(policy.ssl, "create_default_context", return_value=context), \
                patch.object(policy.socket, "create_connection"):
            return policy.probe("test.invalid", version, 1, **kwargs)[0]

    def test_tls12_rejected_by_default_even_with_strong_cipher(self):
        self.assertFalse(self.negotiated("ECDHE-RSA-AES128-GCM-SHA256"))

    def test_tls13_accepted(self):
        self.assertTrue(self.negotiated("TLS_AES_256_GCM_SHA384", ssl.TLSVersion.TLSv1_3))

    def test_tls12_policy_requires_forward_secrecy_and_aead(self):
        for cipher, accepted in (("AES128-GCM-SHA256", False),
                                 ("ECDHE-RSA-AES128-SHA", False),
                                 ("ECDHE-RSA-AES128-GCM-SHA256", True)):
            with self.subTest(cipher=cipher):
                self.assertEqual(self.negotiated(cipher, minimum=ssl.TLSVersion.TLSv1_2), accepted)

    def test_explicit_weak_cipher_acceptance_fails(self):
        for cipher in policy.FORBIDDEN_CIPHERS:
            with self.subTest(cipher=cipher):
                self.assertFalse(self.negotiated(cipher, minimum=ssl.TLSVersion.TLSv1_2,
                                                 cipher=cipher))

    def test_only_remote_rejections_pass_negative_probes(self):
        for reason in (*policy.REMOTE_REJECTIONS, "NO_CIPHERS_AVAILABLE", "CERTIFICATE_VERIFY_FAILED"):
            error = ssl.SSLError(reason)
            error.reason = reason
            with self.subTest(reason=reason), patch.object(policy.ssl, "create_default_context",
                                                          side_effect=error):
                passed, _ = policy.probe("test.invalid", ssl.TLSVersion.TLSv1_2, 1)
                self.assertEqual(passed, reason in policy.REMOTE_REJECTIONS)

    def test_network_failure_never_proves_rejection(self):
        for error in (socket.gaierror("DNS failed"), TimeoutError("timed out")):
            with patch.object(policy.socket, "create_connection", side_effect=error):
                self.assertFalse(policy.probe("test.invalid", ssl.TLSVersion.TLSv1_2, 1)[0])


if __name__ == "__main__":
    unittest.main()
