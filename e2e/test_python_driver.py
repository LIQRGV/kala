#!/usr/bin/env python3
"""Opt-in official-driver tests. Run through ./e2e/run.sh, never cargo test."""
import concurrent.futures
import os
import select
import socket
import subprocess
import threading
import unittest
from urllib.parse import urlparse

import neo4j
from neo4j import GraphDatabase
from neo4j.exceptions import AuthError, ClientError
from neo4j.graph import Node, Relationship

URI = os.environ["KALA_BOLT_URI"]
PREFIX = "// kala:datalog\n"
ALLIES = PREFIX + "[:find ?a :at -210 :where [:edwin :ally_of ?a]]"
NAMES = PREFIX + "[:find ?n :at -210 :where [?e :name ?n]]"


def read_exact(stream, count):
    data = bytearray()
    while len(data) < count:
        block = stream.recv(count - len(data))
        if not block:
            raise EOFError("connection closed during handshake")
        data.extend(block)
    return bytes(data)


AUTH_PASSWORD = "unique-e2e-test-only-password-928164"


def account_command(action, *args, password=None):
    result = subprocess.run(
        [os.environ["KALA_BINARY"], "user", action, os.path.dirname(os.environ["KALA_WORLD"]), *args],
        input=(password + "\n") if password is not None else None,
        text=True, capture_output=True, timeout=30,
    )
    if AUTH_PASSWORD in result.stdout + result.stderr or "$argon2" in result.stdout + result.stderr:
        raise AssertionError("account command leaked a credential")
    if result.returncode:
        raise AssertionError("account command failed: " + result.stderr)
    return result.stdout


class AccountsTests(unittest.TestCase):
    def test_password_grants_revocation_and_root_switch(self):
        with GraphDatabase.driver(URI, auth=None, connection_timeout=5) as driver:
            driver.verify_connectivity()
        account_command("add", "alice@127.0.0.1", "--password-stdin", password=AUTH_PASSWORD)
        account_command("grant", "alice@127.0.0.1", "read", "varrow")
        with GraphDatabase.driver(URI, auth=neo4j.basic_auth("alice", AUTH_PASSWORD), connection_timeout=5) as driver:
            driver.verify_connectivity()
            with driver.session(database="varrow") as session:
                self.assertEqual(len(list(session.run(ALLIES))), 2)
            for database in ["system", "ungranted"]:
                with driver.session(database=database) as session:
                    with self.assertRaises(ClientError) as error:
                        list(session.run(ALLIES))
                    self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
            account_command("revoke", "alice@127.0.0.1", "read", "varrow")
            # A logged-in session keeps its grants until it logs off.
            with driver.session(database="varrow") as session:
                self.assertEqual(len(list(session.run(ALLIES))), 2)
        with GraphDatabase.driver(URI, auth=neo4j.basic_auth("alice", AUTH_PASSWORD), connection_timeout=5) as driver:
            with driver.session(database="varrow") as session:
                with self.assertRaises(ClientError) as error:
                    list(session.run(ALLIES))
                self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
        with GraphDatabase.driver(URI, auth=neo4j.basic_auth("alice", "incorrect-test-password"), connection_timeout=5) as driver:
            with self.assertRaises(AuthError) as error:
                driver.verify_connectivity()
            self.assertEqual(error.exception.code, "Neo.ClientError.Security.Unauthorized")
        account_command("set", "root@localhost", "--password-stdin", password=AUTH_PASSWORD)
        try:
            with GraphDatabase.driver(URI, auth=None, connection_timeout=5) as driver:
                with self.assertRaises(AuthError):
                    driver.verify_connectivity()
            with GraphDatabase.driver(URI, auth=neo4j.basic_auth("root", AUTH_PASSWORD), connection_timeout=5) as driver:
                with driver.session(database="system") as session:
                    names = [row[0] for row in session.run(PREFIX + "[:find ?u :where [?e :user ?u]]")]
                    self.assertIn("root", names)
                    for query in [
                        "[:find ?p :where [?e :password_hash ?p]]",
                        '[:find ?u :where [?e :user ?u] [?e :password_hash ?p] [(< ?p "$argon2z")]]',
                    ]:
                        with self.assertRaises(ClientError) as error:
                            list(session.run(PREFIX + query))
                        self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
                    rows = list(session.run(PREFIX + '[:find ?e :where [?e :user "alice"]]'))
                    self.assertTrue(bool(rows))
                    self.assertTrue("$argon2" not in repr(rows), "hash leaked in query result")
        finally:
            account_command("set", "root@localhost", "--trust")
        with GraphDatabase.driver(URI, auth=None, connection_timeout=5) as driver:
            driver.verify_connectivity()
        for name in ["KALA_SERVER_STDOUT", "KALA_SERVER_STDERR"]:
            with open(os.environ[name], encoding="utf-8") as captured:
                output = captured.read()
            self.assertTrue(AUTH_PASSWORD not in output, "password leaked in server output")
            self.assertTrue("$argon2" not in output, "hash leaked in server output")


class DriverTests(unittest.TestCase):
    def setUp(self):
        self.driver = GraphDatabase.driver(URI, auth=None, connection_timeout=5)
        self.addCleanup(self.driver.close)

    def test_connectivity_and_actual_driver_handshake(self):
        # This byte-transparent proxy records only the public 20-byte handshake,
        # never message payloads or authentication tokens.
        endpoint = urlparse(URI)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            listener.settimeout(10)
            proposals = concurrent.futures.Future()
            relay_done = concurrent.futures.Future()

            def relay():
                try:
                    incoming, _ = listener.accept()
                    with incoming, socket.create_connection((endpoint.hostname, endpoint.port), 5) as upstream:
                        initial = read_exact(incoming, 20)
                        upstream.sendall(initial)
                        selected = read_exact(upstream, 4)
                        incoming.sendall(selected)
                        proposals.set_result((initial, selected))
                        while True:
                            ready, _, _ = select.select([incoming, upstream], [], [], 10)
                            if not ready:
                                raise TimeoutError("driver proxy stalled")
                            for source in ready:
                                data = source.recv(65536)
                                if not data:
                                    relay_done.set_result(None)
                                    return
                                destination = upstream if source is incoming else incoming
                                destination.sendall(data)
                except Exception as error:
                    if not proposals.done():
                        proposals.set_exception(error)
                    relay_done.set_exception(error)

            worker = threading.Thread(target=relay, daemon=True)
            worker.start()
            proxy_uri = "bolt://127.0.0.1:%d" % listener.getsockname()[1]
            try:
                with GraphDatabase.driver(proxy_uri, auth=None, connection_timeout=5) as driver:
                    driver.verify_connectivity()
                    info = driver.get_server_info()
                    self.assertEqual(info.protocol_version, (5, 4))
            finally:
                initial, selected = proposals.result(timeout=10)
                print("driver=%s handshake=%s selected=%s" %
                      (neo4j.__version__, initial.hex(" "), selected.hex(" ")), flush=True)
                worker.join(timeout=10)
            relay_done.result(timeout=1)
            self.assertEqual(initial[:4], bytes.fromhex("60 60 b0 17"))
            self.assertEqual(selected, bytes.fromhex("00 00 04 05"))

    def test_nodes_and_summary(self):
        with self.driver.session(database="varrow") as session:
            result = session.run(ALLIES)
            self.assertEqual(result.keys(), ["a"])
            nodes = [record["a"] for record in result]
            self.assertEqual([node["name"] for node in nodes], ["Greg", "Tomas"])
            for node in nodes:
                self.assertIsInstance(node, Node)
                self.assertIn("CHARACTER", node.labels)
                self.assertTrue(node.element_id.startswith("CHARACTER#"))
                self.assertIn("handle", node)
                self.assertNotIn("located_in", node)
            summary = result.consume()
            self.assertEqual(summary.database, "varrow")
            self.assertEqual(summary.query_type, "r")
            self.assertGreaterEqual(summary.result_consumed_after, 0)

    def test_node_properties_follow_story_tick_and_canon(self):
        with self.driver.session(database="varrow") as session:
            query = PREFIX + '[:find ?e :at -210 :where [?e :handle "edwin"]]'
            earlier = session.run(query).single()[0]
            self.assertIn("Knight", earlier.labels)
            self.assertNotIn("King", earlier.labels)
            self.assertEqual(earlier["name"], "Edwin")
            self.assertEqual(earlier["alias"], ["Ed", "Eddie", "the Lost Knight"])
            query = PREFIX + '[:find ?e :at -180 :where [?e :handle "edwin"]]'
            later = session.run(query).single()[0]
            self.assertIn("King", later.labels)
            self.assertEqual(later["name"], "the Grey King")
            query = PREFIX + '[:find ?e :at -210 :canon 5 :where [?e :handle "edwin"]]'
            old_canon = session.run(query).single()[0]
            self.assertEqual(old_canon["alias"], ["Ed", "Eddy", "the Lost Knight"])

    def test_basic_auth(self):
        with GraphDatabase.driver(URI, auth=("test-user", "fake-driver-password")) as driver:
            driver.verify_connectivity()
            with driver.session(database="varrow") as session:
                self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_default_database(self):
        with self.driver.session() as session:
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_parameters_and_ignored_extra(self):
        query = PREFIX + "[:find ?a :in $ ?who ?t :at ?t :where [?e :handle ?who] [?e :ally_of ?a]]"
        with self.driver.session(database="varrow") as session:
            nodes = [r["a"] for r in session.run(query, who="edwin", t=-210, unused=None)]
            self.assertEqual([n["name"] for n in nodes], ["Greg", "Tomas"])

    def test_parameter_errors_and_session_reuse(self):
        query = PREFIX + "[:find ?a :in $ ?who ?t :at ?t :where [?e :handle ?who] [?e :ally_of ?a]]"
        with self.driver.session(database="varrow") as session:
            for params, code in [({"who": "edwin"}, "ParameterMissing"),
                                 ({"who": "edwin", "t": "-210"}, "TypeError"),
                                 ({"who": None, "t": -210}, "TypeError")]:
                with self.subTest(code=code, params=params):
                    with self.assertRaises(ClientError) as error:
                        list(session.run(query, params))
                    self.assertEqual(error.exception.code, "Neo.ClientError.Statement." + code)
                    self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_aggregate_history_and_intervals(self):
        with self.driver.session(database="varrow") as session:
            query = PREFIX + "[:find (count ?c) :at -215 :where [?c :located_in :varrow]]"
            result = session.run(query)
            self.assertEqual(result.keys(), ["(count ?c)"])
            count = result.single()[0]
            self.assertIs(type(count), int)
            self.assertEqual(count, 5)
            query = PREFIX + "[:find ?t ?op ?p :where [:mara :name ?n ?t ?op ?p]]"
            rows = [tuple(row) for row in session.run(query)]
            self.assertIn((-100, "retract", "decade"), rows)
            query = PREFIX + "[:find ?s :where (during [:osric :located_in :varrow] ?s)]"
            self.assertEqual(session.run(query).single()[0], [[-400, None]])

    def test_relationship_hydration(self):
        with self.driver.session(database="varrow") as session:
            query = PREFIX + "[:find ?e :at -230 :where [?e :disguised true]]"
            relation = session.run(query).single()[0]
            self.assertIsInstance(relation, Relationship)
            self.assertEqual(relation.type, "present")
            self.assertTrue(relation["disguised"])
            self.assertTrue(relation.element_id.startswith("edge#"))
            self.assertTrue(relation.start_node.element_id.startswith("CHARACTER#"))
            self.assertTrue(relation.end_node.element_id.startswith("PLACE#"))

    def test_fetch_size_five_and_discard(self):
        with self.driver.session(database="varrow", fetch_size=5) as session:
            rows = [r["n"] for r in session.run(NAMES)]
            self.assertGreaterEqual(len(rows), 30)
            self.assertEqual(len(rows), len(set(rows)))
            with self.driver.session(database="varrow", fetch_size=1000) as other:
                self.assertEqual(rows, [r["n"] for r in other.run(NAMES)])
            result = session.run(NAMES)
            next(iter(result))
            summary = result.consume()  # DISCARD the remaining buffered stream
            self.assertEqual(summary.database, "varrow")
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_syntax_language_errors_and_same_session_reuse(self):
        with self.driver.session(database="varrow") as session:
            for query, located in [(PREFIX + "[:find", True), ("RETURN 1", False),
                                   ("// kala:gremlin\ng.V()", False)]:
                with self.subTest(query=query):
                    with self.assertRaises(ClientError) as error:
                        list(session.run(query))
                    self.assertEqual(error.exception.code, "Neo.ClientError.Statement.SyntaxError")
                    if located:
                        self.assertIn("line", error.exception.message)
                        self.assertIn("column", error.exception.message)
                    else:
                        self.assertIn("not supported yet", error.exception.message)
                    self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_unknown_database(self):
        with self.driver.session(database="nope") as session:
            with self.assertRaises(ClientError) as error:
                list(session.run(ALLIES))
            self.assertEqual(error.exception.code, "Neo.ClientError.Database.DatabaseNotFound")
        with self.driver.session(database="varrow") as session:
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_explicit_transaction_refused_and_session_stays_usable(self):
        with self.driver.session(database="varrow") as session:
            with self.assertRaises(ClientError) as error:
                session.begin_transaction()
            self.assertEqual(error.exception.code, "Neo.ClientError.Request.Invalid")
            self.assertIn("not supported yet", error.exception.message)
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_external_cli_commit_visible_without_restart(self):
        query = PREFIX + "[:find ?a :at 700 :where [:greg :alias ?a]]"
        with self.driver.session(database="varrow") as session:
            before = [r[0] for r in session.run(query)]
            self.assertNotIn("Greg the Driver-Tested", before)
            result = subprocess.run([os.environ["KALA_BINARY"], "transact", os.environ["KALA_WORLD"], "-"],
                                    input='[:assert :greg :alias "Greg the Driver-Tested" :at 700]\n',
                                    text=True, capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            after = [r[0] for r in session.run(query)]
            self.assertEqual(set(after), set(before) | {"Greg the Driver-Tested"})


if __name__ == "__main__":
    print("Official Python driver: neo4j==" + neo4j.__version__, flush=True)
    unittest.main(verbosity=2)
