"""Connectivity, certificate verification and rotation with the official driver."""
import hashlib
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import ssl
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

from neo4j import GraphDatabase
from neo4j.exceptions import ServiceUnavailable


@unittest.skipUnless(os.getenv("KALA_TLS_FIXTURES"), "OpenSSL test certificates unavailable")
class TlsDriverTests(unittest.TestCase):
    def start(self, cert="self"):
        self.fixtures = Path(os.environ["KALA_TLS_FIXTURES"])
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        shutil.copy(os.environ["KALA_WORLD"], self.directory / "varrow.kala")
        self.install_pair(cert)
        self.log = self.directory / "server.stderr"
        with self.log.open("w") as errors:
            self.process = subprocess.Popen(
                [os.environ["KALA_BINARY"], "serve", self.directory, "--default-db", "varrow",
                 "--listen", "bolt+s://127.0.0.1:0", "--tls.cert", self.directory / "cert.pem",
                 "--tls.key", self.directory / "key.pem"],
                stdout=subprocess.PIPE, stderr=errors, text=True,
            )
        self.addCleanup(self.stop)
        ready, _, _ = select.select([self.process.stdout], [], [], 10)
        self.assertTrue(ready, "TLS server did not announce its address")
        prefix = "kala serve: listening on bolt+s://127.0.0.1:"
        line = self.process.stdout.readline().strip()
        self.assertTrue(line.startswith(prefix), self.log.read_text())
        self.port = int(line[len(prefix):])

    def install_pair(self, name):
        shutil.copy(self.fixtures / (name + ".pem"), self.directory / "cert.pem")
        shutil.copy(self.fixtures / (name + ".key"), self.directory / "key.pem")

    def stop(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
        try:
            self.process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.communicate()
            raise

    def driver(self, scheme, host="127.0.0.1"):
        return GraphDatabase.driver(f"{scheme}://{host}:{self.port}", auth=None,
                                    connection_timeout=3, max_transaction_retry_time=0)

    def test_direct_and_routed_queries_and_plaintext_refusal(self):
        self.start()
        with self.driver("bolt") as plain:
            with self.assertRaises(ServiceUnavailable):
                plain.verify_connectivity()
        with self.driver("bolt+ssc") as direct, direct.session(database="varrow") as session:
            session.execute_write(lambda tx: tx.run(
                "CREATE (:CHARACTER {kala_handle:'over_tls',name:'TLS writer'}) AT TICK 1000"
            ).consume())
            self.assertEqual(session.run(
                "MATCH (n {kala_handle:'over_tls'}) AT TICK 1000 RETURN n.name"
            ).single()[0], "TLS writer")
        with self.driver("neo4j+ssc") as routing, routing.session(database="varrow") as session:
            self.assertEqual(session.execute_read(lambda tx: tx.run(
                "MATCH (n {kala_handle:'over_tls'}) AT TICK 1000 RETURN n.name"
            ).single()[0]), "TLS writer")
            addresses = {row[0] for row in session.run("SHOW DATABASES YIELD address")}
            self.assertEqual(addresses, {f"127.0.0.1:{self.port}"})
        self.assertIn("event=bolt_connection_dropped reason=tls_handshake", self.log.read_text())

    def test_verified_driver_checks_the_ca_and_both_subject_alt_names(self):
        self.start("signed")
        with self.driver("bolt+s") as untrusted:
            with self.assertRaises(ServiceUnavailable):
                untrusted.verify_connectivity()
        # neo4j 5.28 rejects trusted_certificates= on +s URIs. Its SSL context
        # calls load_default_certs(), which honors the process-local CA file.
        with patch.dict(os.environ, SSL_CERT_FILE=str(self.fixtures / "ca.pem")):
            for host in ("127.0.0.1", "localhost"):
                with self.subTest(host=host), self.driver("bolt+s", host) as driver:
                    driver.verify_connectivity()
            # Deliver a third hostname to the same socket without changing the
            # hostname supplied to the driver's TLS verifier or relying on DNS.
            resolve = socket.getaddrinfo
            def local_dns(host, *args, **kwargs):
                return resolve("127.0.0.1" if host == "wrong-san.test" else host, *args, **kwargs)
            with patch("socket.getaddrinfo", local_dns), self.driver("bolt+s", "wrong-san.test") as driver:
                with self.assertRaises(ServiceUnavailable) as failure:
                    driver.verify_connectivity()
                cause = failure.exception
                while cause and not isinstance(cause, ssl.SSLCertVerificationError):
                    cause = cause.__cause__
                self.assertIsInstance(cause, ssl.SSLCertVerificationError)
                self.assertIn("hostname mismatch", cause.verify_message.lower())

    def fingerprint(self):
        context = ssl.create_default_context(cafile=self.fixtures / "ca.pem")
        context.load_verify_locations(self.fixtures / "self.pem")
        with socket.create_connection(("127.0.0.1", self.port), timeout=3) as raw:
            with context.wrap_socket(raw, server_hostname="localhost") as tls:
                self.assertIsNone(tls.selected_alpn_protocol())
                return hashlib.sha256(tls.getpeercert(binary_form=True)).digest()

    def await_condition(self, predicate, description):
        deadline = time.monotonic() + 5
        while not predicate():
            self.assertLess(time.monotonic(), deadline, description)
            time.sleep(0.02)

    def test_sighup_rotates_new_connections_and_retains_last_good_pair(self):
        self.start()
        def expected(name):
            der = ssl.PEM_cert_to_DER_cert((self.fixtures / (name + ".pem")).read_text())
            return hashlib.sha256(der).digest()

        self.assertEqual(self.fingerprint(), expected("self"))
        with self.driver("bolt+ssc") as driver, driver.session(database="varrow") as session:
            # An explicit transaction pins this TLS connection through both reloads.
            with session.begin_transaction() as tx:
                query = "MATCH (n {kala_handle:'varrow'}) AT TICK -210 RETURN n.name"
                self.assertEqual(tx.run(query).single()[0], "Varrow")
                self.install_pair("signed")
                self.process.send_signal(signal.SIGHUP)
                rotated = expected("signed")
                self.await_condition(lambda: self.fingerprint() == rotated, "certificate did not rotate")
                self.assertEqual(tx.run(query).single()[0], "Varrow")
                marker = "INVALID-TEST-KEY-CONTENTS"
                (self.directory / "key.pem").write_text(marker)
                self.process.send_signal(signal.SIGHUP)
                def failures():
                    return [line for line in self.log.read_text().splitlines()
                            if "reload" in line.lower() and ("fail" in line.lower() or "error" in line.lower())]
                self.await_condition(failures, "failed reload was not reported")
                self.assertEqual(self.fingerprint(), rotated)
                self.assertEqual(tx.run(query).single()[0], "Varrow")
                self.assertEqual(len(failures()), 1)
                self.assertFalse(marker in self.log.read_text(), "failed reload logged private key contents")

    def test_sighup_refuses_oversized_files_and_preserves_identity(self):
        self.start()
        original = self.fingerprint()
        for name in ("cert.pem", "key.pem"):
            with self.subTest(file=name):
                self.install_pair("signed")
                path = self.directory / name
                pem = path.read_bytes()
                path.write_bytes(pem.ljust(1024 * 1024 + 1, b" "))
                offset = len(self.log.read_text())
                self.process.send_signal(signal.SIGHUP)
                def reload_log():
                    return [line for line in self.log.read_text()[offset:].splitlines()
                            if "event=bolt_tls_reload" in line]
                self.await_condition(reload_log, "no outcome for oversized file reload")
                lines = reload_log()
                self.assertEqual(len(lines), 1)
                diagnostic = lines[0]
                self.assertTrue("event=bolt_tls_reload_failed" in diagnostic,
                                f"reload accepted {name} over 1 MiB")
                self.assertTrue(name in diagnostic, "size error omitted filename")
                self.assertTrue("1 MiB" in diagnostic or "1048576" in diagnostic,
                                "reload omitted size limit")
                self.assertEqual(self.fingerprint(), original)
