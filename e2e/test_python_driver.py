#!/usr/bin/env python3
"""Opt-in official-driver tests. Run through ./e2e/run.sh, never cargo test."""
import concurrent.futures
import contextlib
import os
import select
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import urlparse

import neo4j
from neo4j import GraphDatabase
from neo4j.exceptions import AuthError, ClientError, CypherSyntaxError
from neo4j.graph import Node, Relationship, Path

from test_tls_driver import TlsDriverTests

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

    def test_plain_cypher_nodes_parameters_and_datalog_prefix(self):
        with self.driver.session(database="varrow") as session:
            query = "MATCH (p {kala_handle: 'varrow'}) AT TICK $tick CANON $canon RETURN p"
            node = session.run(query, tick=-210, canon=10).single()[0]
            self.assertIsInstance(node, Node)
            self.assertEqual(node.labels, frozenset(["PLACE", "City"]))
            self.assertEqual(node["name"], "Varrow")
            self.assertEqual(node["kala_handle"], "varrow")
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_plain_cypher_relationship_null_list_order_and_syntax(self):
        with self.driver.session(database="varrow") as session:
            row = session.run("MATCH (e {kala_handle: 'edwin'})-[r:present]->(p) AT TICK -240 RETURN r, r.disguised, p.name").single()
            rel = row[0]
            self.assertIsInstance(rel, Relationship)
            self.assertEqual(rel.type, "present")
            self.assertEqual(rel.element_id, "edge#98")
            self.assertTrue(rel["disguised"])
            self.assertEqual(list(row[1:]), [True, "Carnath"])
            self.assertIsNone(session.run("MATCH (c {kala_handle: 'kael'}) RETURN c.biography").single()[0])
            self.assertEqual(session.run("MATCH (e {kala_handle: 'edwin'}) AT TICK -210 RETURN e.alias").single()[0],
                             ["Ed", "Eddie", "the Lost Knight"])
            names = [r[0] for r in session.run("MATCH (c:CHARACTER) RETURN c.name ORDER BY c.name SKIP 2 LIMIT 2")]
            self.assertEqual(names, ["Hild", "Lisa"])
            with self.assertRaises(CypherSyntaxError) as error:
                list(session.run("MATCH (c) RETURN properties(c)"))
            self.assertIn("properties", error.exception.message)
            self.assertIn("line", error.exception.message)
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_cypher_list_and_null_parameters(self):
        with self.driver.session(database="varrow") as session:
            aliases = ["Ed", "Eddie", "the Lost Knight"]
            query = "MATCH (e {kala_handle: 'edwin'}) AT TICK -210 WHERE e.alias = $aliases RETURN e.name"
            self.assertEqual(session.run(query, aliases=aliases).single()[0], "Edwin")
            self.assertEqual(list(session.run(query, aliases=list(reversed(aliases)))), [])
            query = "MATCH (e {kala_handle: 'edwin'}) RETURN $value"
            self.assertIsNone(session.run(query, value=None).single()[0])
            nested = [[1, None], ["x", True]]
            self.assertEqual(session.run(query, value=nested).single()[0], nested)

    def test_cypher_system_secret_refusal_and_node_redaction(self):
        with self.driver.session(database="system") as session:
            self.assertIn("root", [r[0] for r in session.run("MATCH (a:ACCOUNT) RETURN a.user")])
            for query in [
                "MATCH (a) RETURN a.password_hash",
                "MATCH (a) RETURN a.`password_hash` AS x",
                "MATCH (a) WHERE a.password_hash = 'x' RETURN a.user",
                "MATCH (a) WHERE a.password_hash IS NULL RETURN a.user",
                "MATCH (a) RETURN a.user ORDER BY a.password_hash LIMIT 1",
                "MATCH (a {password_hash: 'x'}) RETURN a",
                "MATCH (a)-[r:missing {password_hash: 'x'}]->(b) RETURN a",
            ]:
                with self.subTest(query=query):
                    with self.assertRaises(ClientError) as error:
                        list(session.run(query))
                    self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
                    self.assertNotIn("$argon2", error.exception.message)
            for row in session.run("MATCH (a:ACCOUNT) RETURN a"):
                self.assertIsInstance(row[0], Node)
                if "password_hash" in row[0]:
                    self.assertEqual(row[0]["password_hash"], "<redacted>")

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
                self.assertIn("kala_handle", node)
                self.assertNotIn("handle", node)
                self.assertNotIn("located_in", node)
            summary = result.consume()
            self.assertEqual(summary.database, "varrow")
            self.assertEqual(summary.query_type, "r")
            self.assertGreaterEqual(summary.result_consumed_after, 0)

    def test_node_properties_follow_story_tick_and_canon(self):
        with self.driver.session(database="varrow") as session:
            query = PREFIX + '[:find ?e :at -210 :where [?e :kala_handle "edwin"]]'
            earlier = session.run(query).single()[0]
            self.assertIn("Knight", earlier.labels)
            self.assertNotIn("King", earlier.labels)
            self.assertEqual(earlier["name"], "Edwin")
            self.assertEqual(earlier["kala_handle"], "edwin")
            self.assertEqual(earlier["alias"], ["Ed", "Eddie", "the Lost Knight"])
            query = PREFIX + '[:find ?e :at -180 :where [?e :kala_handle "edwin"]]'
            later = session.run(query).single()[0]
            self.assertIn("King", later.labels)
            self.assertEqual(later["name"], "the Grey King")
            query = PREFIX + '[:find ?e :at -210 :canon 5 :where [?e :kala_handle "edwin"]]'
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
        query = PREFIX + "[:find ?a :in $ ?who ?t :at ?t :where [?e :kala_handle ?who] [?e :ally_of ?a]]"
        with self.driver.session(database="varrow") as session:
            nodes = [r["a"] for r in session.run(query, who="edwin", t=-210, unused=None)]
            self.assertEqual([n["name"] for n in nodes], ["Greg", "Tomas"])

    def test_parameter_errors_and_session_reuse(self):
        query = PREFIX + "[:find ?a :in $ ?who ?t :at ?t :where [?e :kala_handle ?who] [?e :ally_of ?a]]"
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

    def test_explicit_read_transaction_and_session_stays_usable(self):
        with self.driver.session(database="varrow") as session:
            with session.begin_transaction() as transaction:
                self.assertEqual(len(list(transaction.run(ALLIES))), 2)
                transaction.commit()
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

    def test_cypher_aggregates_optional_with_and_unwind(self):
        with self.driver.session(database="varrow") as session:
            row = session.run("MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN count(*), count(DISTINCT a)").single()
            self.assertEqual(list(row), [30, 28])
            row = session.run("MATCH (a)-[:ally_of]->(b) AT TICK -208 WITH a, count(b) AS n WHERE n > 1 RETURN a.name, n").single()
            self.assertEqual(list(row), ["Edwin", 3])
            row = session.run("MATCH (c {kala_handle: 'nobody'}) RETURN count(*), collect(c.name), min(c.name), sum(1), avg(1)").single()
            self.assertEqual(list(row), [0, [], None, 0, None])
            row = session.run("MATCH (c {kala_handle: 'kael'}) OPTIONAL MATCH (c)-[:ally_of]->(a) RETURN c.name, a").single()
            self.assertEqual(list(row), ["Edwin", None])
            rows = session.run("UNWIND $names AS n MATCH (c:CHARACTER {name:n}) RETURN c.kala_handle ORDER BY c.kala_handle", names=["Greg", "Hild", "Nobody"])
            self.assertEqual([r[0] for r in rows], ["greg", "hild"])
            row = session.run("UNWIND [3, null, 1, 3] AS x RETURN collect(x), count(*), count(x)").single()
            self.assertEqual(list(row), [[3, 1, 3], 4, 3])

    def test_path_hydration_and_bounded_relationship_errors(self):
        with self.driver.session(database="varrow") as session:
            rows = list(session.run("MATCH p = (e {kala_handle: 'edwin'})-[r:ally_of*1..2]->(x) AT TICK -208 RETURN p, length(p), nodes(p), relationships(p), r"))
            self.assertEqual(len(rows), 3)
            for row in rows:
                path, length, nodes, rels, rel_list = row
                self.assertIsInstance(path, Path)
                self.assertEqual(length, 1)
                self.assertEqual(path.start_node["kala_handle"], "edwin")
                self.assertIn(path.end_node["kala_handle"], ["greg", "mara", "tomas"])
                self.assertEqual(list(path.nodes), nodes)
                self.assertEqual(list(path.relationships), rels)
                self.assertEqual(rels, rel_list)
                self.assertEqual(len(set(r.element_id for r in rels)), len(rels))
                for rel in rels:
                    self.assertEqual(rel.type, "ally_of")
                    self.assertEqual({rel.start_node.element_id, rel.end_node.element_id}, {n.element_id for n in nodes})
            # Every bidirectional edge is traversed in both directions. The
            # driver's relationship objects must retain the same stored ends.
            ends = {}
            for row in session.run("MATCH p = (a)-[:ally_of]-(b) AT TICK -208 RETURN p"):
                path = row[0]
                rel = path.relationships[0]
                pair = (rel.start_node.element_id, rel.end_node.element_id)
                self.assertEqual(ends.setdefault(rel.element_id, pair), pair)
            self.assertEqual(len(ends), 15)
            path = session.run("MATCH p = (e {kala_handle: 'edwin'})-[:present]->(c) AT TICK -240 RETURN p").single()[0]
            self.assertIsInstance(path, Path)
            self.assertEqual(path.start_node["kala_handle"], "edwin")
            self.assertEqual(path.end_node["kala_handle"], "carnath")
            self.assertTrue(path.relationships[0]["disguised"])
            zero = session.run("MATCH p = (a {kala_handle: 'edwin'}) RETURN p").single()[0]
            self.assertIsInstance(zero, Path)
            self.assertEqual(zero.start_node, zero.end_node)
            self.assertEqual(len(zero.relationships), 0)
            for bound in ["*", "*2..", "*1..11"]:
                with self.assertRaises(CypherSyntaxError) as error:
                    list(session.run("MATCH (a)-[:ally_of%s]-(b) RETURN b" % bound))
                self.assertIn("10", error.exception.message)

    def test_procedures_and_show_database_visibility(self):
        with self.driver.session(database="varrow") as session:
            row = session.run("CALL dbms.components()").single()
            self.assertEqual(dict(row), {"name": "Kala", "versions": ["5.0.0"], "edition": "community"})
            labels = [r[0] for r in session.run("CALL db.labels()")]
            self.assertTrue({"CHARACTER", "PLACE", "City"}.issubset(labels))
            rels = [r[0] for r in session.run("CALL db.relationshipTypes()")]
            self.assertTrue({"ally_of", "located_in"}.issubset(rels))
            props = [r[0] for r in session.run("CALL db.propertyKeys()")]
            self.assertTrue({"name", "kala_handle"}.issubset(props))
            self.assertNotIn("ally_of", props)
            self.assertEqual([r[0] for r in session.run("CALL db.labels() YIELD label WHERE label = 'City' RETURN label")], ["City"])
        with GraphDatabase.driver(URI, auth=("test-user", ""), connection_timeout=5) as driver:
            for db in ["varrow", "system"]:
                with driver.session(database=db) as session:
                    rows = [dict(r) for r in session.run("SHOW DATABASES")]
                    self.assertEqual([r["name"] for r in rows], ["varrow"])
                    self.assertEqual(rows[0]["access"], "read-only")
                    self.assertTrue(rows[0]["writer"])
                    self.assertTrue(rows[0]["default"])
        with self.driver.session(database="system") as session:
            self.assertEqual([r[0] for r in session.run("SHOW DATABASES YIELD name")], ["hidden", "system", "varrow"])
            with self.assertRaises(ClientError) as error:
                list(session.run("MATCH (a:ACCOUNT) RETURN count(a.password_hash)"))
            self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
            for record in session.run("MATCH p = (a:ACCOUNT) RETURN p"):
                for node in record[0].nodes:
                    if "password_hash" in node:
                        self.assertEqual(node["password_hash"], "<redacted>")

    def test_routing_driver_verifies_and_runs_autocommit_with_and_without_db(self):
        routing_uri = URI.replace("bolt://", "neo4j://", 1)
        with GraphDatabase.driver(routing_uri, auth=("test-user", ""), connection_timeout=5) as driver:
            driver.verify_connectivity()
            for config in [{"database": "varrow"}, {}]:
                with driver.session(**config) as session:
                    self.assertEqual(session.run("MATCH (a)-[:ally_of]->(b) AT TICK -208 RETURN count(*)").single()[0], 30)
                    self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_routing_driver_uses_advertised_address(self):
        # A resolver maps the advertised address to the server's ephemeral
        # socket. This tests an advertised hostname without reserving a port.
        with tempfile.TemporaryDirectory() as directory:
            shutil.copyfile(os.environ["KALA_WORLD"], os.path.join(directory, "varrow.kala"))
            process = subprocess.Popen([os.environ["KALA_BINARY"], "serve", directory, "--auth", "none", "--listen", "127.0.0.1:0", "--advertised-address", "localhost:7687"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                ready, _, _ = select.select([process.stdout], [], [], 20)
                self.assertTrue(ready, "advertised-address server did not announce readiness")
                line = process.stdout.readline().strip()
                if not line.startswith("kala serve: listening on 127.0.0.1:"):
                    _, errors = process.communicate(timeout=15)
                    self.fail("advertised-address server did not start: " + line + errors)
                endpoint = urlparse("bolt://" + line.split("listening on ", 1)[1])
                seen = []
                def resolver(address):
                    seen.append(tuple(address))
                    return [("127.0.0.1", endpoint.port)]
                with GraphDatabase.driver("neo4j://127.0.0.1:%d" % endpoint.port, auth=None, resolver=resolver, connection_timeout=5) as driver:
                    driver.verify_connectivity()
                    with driver.session(database="varrow") as session:
                        self.assertEqual(len(list(session.run(ALLIES))), 2)
                        row = session.run("SHOW DATABASES YIELD address").single()
                        self.assertEqual(row[0], "localhost:7687")
                self.assertIn(("localhost", 7687), seen)
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                process.communicate(timeout=15)


class WriteDriverTests(unittest.TestCase):
    """The writable scratch world is isolated from all original read tests."""

    @classmethod
    def setUpClass(cls):
        cls.directory = os.environ["KALA_WRITE_DIR"]
        cls.stderr = open(os.path.join(cls.directory, "server.stderr"), "w", encoding="utf-8")
        cls.process = subprocess.Popen(
            [os.environ["KALA_BINARY"], "serve", cls.directory, "--listen", "127.0.0.1:0", "--default-db", "varrow"],
            stdout=subprocess.PIPE, stderr=cls.stderr, text=True,
        )
        cls.addClassCleanup(cls.stop_server)
        ready, _, _ = select.select([cls.process.stdout], [], [], 20)
        if not ready:
            raise AssertionError("write server did not announce readiness")
        line = cls.process.stdout.readline().strip()
        if not line.startswith("kala serve: listening on 127.0.0.1:"):
            raise AssertionError("write server did not start: " + line)
        cls.uri = "bolt://" + line.split("listening on ", 1)[1]

    @classmethod
    def stop_server(cls):
        if cls.process.poll() is None:
            cls.process.send_signal(signal.SIGINT)
        cls.process.communicate(timeout=15)
        cls.stderr.close()

    def driver(self, user="writer", routing=False):
        uri = self.uri.replace("bolt://", "neo4j://", 1) if routing else self.uri
        driver = GraphDatabase.driver(
            uri, auth=None if user == "root" else (user, ""), connection_timeout=5,
            max_transaction_retry_time=5, initial_retry_delay=0.01,
        )
        self.addCleanup(driver.close)
        return driver

    def aliases(self, driver):
        with driver.session(database="varrow", default_access_mode=neo4j.READ_ACCESS) as session:
            return [r[0] for r in session.run(PREFIX + "[:find ?a :where [:greg :alias ?a]]")]

    def test_cypher_managed_writes_bookmarks_and_summary_counters(self):
        driver = self.driver(routing=True)
        with driver.session(database="annex") as session:
            def read(query, **params):
                return session.execute_read(lambda tx: [r[0] for r in tx.run(query, **params)])

            session.execute_write(lambda tx: tx.run(
                "CREATE (n:CHARACTER:Scout {kala_handle:$h,name:$name}) AT TICK 8100",
                h="driver_cypher", name="Aster").consume())
            self.assertTrue(session.last_bookmarks().raw_values)
            match = "MATCH (n {kala_handle:'driver_cypher'})"
            self.assertEqual(read(match + " AT TICK 8100 RETURN n.name"), ["Aster"])
            records, summary, _ = driver.execute_query(
                match + " SET n.name = $name AT TICK 8101", name="Captain", database_="annex")
            self.assertEqual(records, [])
            self.assertEqual(summary.query_type, "rw")
            self.assertEqual(read(match + " AT TICK 8101 RETURN n.name"), ["Captain"])

            # Auto-commit has final identities and committed properties before records.
            result = session.run("CREATE (m:CHARACTER:Scout {kala_handle:'driver_cypher_peer', "
                                 "name:'Peer'}) AT TICK 8101 RETURN m")
            node = result.single()[0]
            self.assertIsInstance(node, Node)
            self.assertEqual(node.labels, {"CHARACTER", "Scout"})
            self.assertEqual(node["name"], "Peer")
            counters = result.consume().counters
            self.assertEqual(counters.nodes_created, 1)
            self.assertEqual(counters.labels_added, 1)
            self.assertGreaterEqual(counters.properties_set, 1)

            edge = ("MATCH (a {kala_handle:'driver_cypher'}), "
                    "(b {kala_handle:'driver_cypher_peer'}) MERGE (a)-[r:present]->(b) "
                    "ON CREATE SET r.since = 1 AT TICK 8102")
            for i in range(2):
                summary = session.run(edge).consume()
                self.assertEqual(summary.counters.relationships_created, 1 if i == 0 else 0)
                if i == 1:
                    self.assertEqual(summary.metadata["kala"]["changed"], 0)
            self.assertEqual(read(match + "-[r:present]->(m) AT TICK 8102 RETURN r.since"), [1])
            session.execute_write(lambda tx: tx.run(match + " DETACH DELETE n AT TICK 8103").consume())
            self.assertEqual(read(match + " AT TICK 8103 RETURN n.name"), [None])
            self.assertEqual(read(match + "-[r:present]->(m) AT TICK 8103 RETURN m"), [])
            self.assertEqual(read(match + " AT TICK 8102 RETURN n.name"), ["Captain"])

    def test_cypher_bulk_unwind_merge_is_idempotent(self):
        handles = ["driver_bulk_a", "driver_bulk_b", "driver_bulk_c"]
        create = ("UNWIND $handles AS h CREATE (:CHARACTER {kala_handle:h, name:h}) "
                  "AT TICK 8200")
        merge = ("UNWIND $handles AS h MERGE (n:CHARACTER {kala_handle:h}) "
                 "ON CREATE SET n.name = h AT TICK 8200")
        with self.driver().session(database="annex") as session:
            session.execute_write(lambda tx: tx.run(create, handles=handles).consume())
            def names():
                return session.execute_read(lambda tx: [r[0] for r in tx.run(
                    "UNWIND $handles AS h MATCH (n {kala_handle:h}) RETURN n.name ORDER BY n.name",
                    handles=handles)])
            self.assertEqual(names(), handles)
            for _ in range(2):
                summary = session.run(merge, handles=handles).consume()
                self.assertEqual(summary.metadata["kala"]["changed"], 0)
            self.assertEqual(names(), handles)

    def test_map_row_parameters_write_at_independent_ticks(self):
        rows = [
            {"handle": "py_sparrow", "name": "Sparrow", "tick": -12},
            {"handle": "py_warbler", "name": "Warbler", "tick": 4},
        ]
        query = ("UNWIND $rows AS row MERGE (n:CHARACTER {kala_handle:row.handle}) "
                 "SET n.name=row.name AT TICK row.tick")
        with self.driver().session(database="annex") as session:
            session.execute_write(lambda tx: tx.run(query, rows=rows).consume())
            for row in rows:
                for tick, expected in [(row["tick"] - 1, None), (row["tick"], row["name"])]:
                    actual = session.run(
                        "MATCH (n {kala_handle:$h}) AT TICK $t RETURN n.name",
                        h=row["handle"], t=tick).single()[0]
                    self.assertEqual(actual, expected)

    def test_cypher_parallel_list_merge_with_native_driver_parameters(self):
        query = ("UNWIND $indices AS i MERGE (n:CHARACTER {kala_handle:$handles[i]}) "
                 "ON CREATE SET n.name=$names[i] AT TICK 8240")
        params = {
            "indices": [2, 0, 1],
            "handles": ["parallel_ash", "parallel_elm", "parallel_oak"],
            "names": ["Ash", "Elm", "Oak"],
        }
        with self.driver().session(database="annex") as session:
            session.execute_write(lambda tx: tx.run(query, params).consume())
            first = session.last_bookmarks().raw_values
            for handle, expected in zip(params["handles"], params["names"]):
                actual = session.execute_read(lambda tx: tx.run(
                    "MATCH (n {kala_handle:$handle}) AT TICK 8240 RETURN n.name", handle=handle
                ).single()[0])
                self.assertEqual(actual, expected)
            again = session.run(query, params).consume()
            self.assertEqual(again.metadata["kala"]["changed"], 0)
            self.assertEqual(session.last_bookmarks().raw_values, first)
            for invalid in [
                dict(params, indices=[True]),
                dict(params, names="not a list", handles=["parallel_failed"], indices=[0]),
            ]:
                with self.assertRaises(ClientError) as error:
                    session.execute_write(lambda tx: tx.run(query, invalid).consume())
                self.assertEqual(error.exception.code, "Neo.ClientError.Statement.TypeError")
            self.assertEqual(list(session.run(
                "MATCH (n {kala_handle:'parallel_failed'}) RETURN n")), [])

    def test_cypher_execute_write_retries_conflicting_set(self):
        driver = self.driver()
        match = "MATCH (n {kala_handle:'driver_cypher_counter'})"
        with driver.session(database="annex") as session:
            session.run("CREATE (:CHARACTER {kala_handle:'driver_cypher_counter', points:0}) "
                        "AT TICK 8300").consume()
        attempts = [0, 0]
        gate = threading.Barrier(2, timeout=10)

        def increment(worker):
            with driver.session(database="annex") as session:
                def work(tx):
                    attempts[worker] += 1
                    value = tx.run(match + " RETURN n.points").single()[0]
                    if attempts[worker] == 1:
                        gate.wait()
                    tx.run(match + " SET n.points = $value AT TICK $tick",
                           value=value + 1, tick=8301 + value).consume()
                    return value + 1
                return session.execute_write(work)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            jobs = [pool.submit(increment, i) for i in range(2)]
            self.assertEqual(sorted(j.result(timeout=20) for j in jobs), [1, 2])
        self.assertGreaterEqual(sum(attempts), 3)

    def test_cypher_write_parameters_take_the_kinds_of_the_reading_part(self):
        with self.driver().session(database="annex") as session:
            # A handle string for a ref-typed position finds the same rows as a read.
            found = session.execute_read(lambda tx: [r[0] for r in tx.run(
                "MATCH (c:CHARACTER {located_in:$place}) AT TICK 700 RETURN c.kala_handle",
                place="ashford")])
            self.assertIn("greg", found)
            summary = session.run(
                "MATCH (c:CHARACTER {located_in:$place}) SET c.checked_in = true AT TICK 700",
                place="ashford").consume()
            self.assertEqual(summary.counters.properties_set, len(found))
            self.assertEqual(session.execute_read(lambda tx: tx.run(
                "MATCH (c {kala_handle:'greg'}) AT TICK 700 RETURN c.checked_in").single()[0]), True)
            # A handle string written to a ref attribute resolves to the entity.
            session.execute_write(lambda tx: tx.run(
                "MATCH (c {kala_handle:'wren'}) SET c.located_in = $place AT TICK 8500",
                place="varrow").consume())
            self.assertEqual(session.run(
                "MATCH (c {kala_handle:'wren'})-[:located_in]->(p) AT TICK 8500 "
                "RETURN p.kala_handle").single()[0], "varrow")
            for query, params, code in [
                ("MATCH (c:CHARACTER {located_in:$place}) SET c.checked_in = true", {"place": 5},
                 "Neo.ClientError.Statement.TypeError"),
                ("MATCH (c {kala_handle:'wren'}) SET c.located_in = $place AT TICK 8501",
                 {"place": "no-such-place"}, None),
            ]:
                with self.assertRaises(ClientError) as error:
                    session.run(query, params).consume()
                if code:
                    self.assertEqual(error.exception.code, code)
                else:
                    self.assertIn("unknown entity `no-such-place`", error.exception.message)

    def test_refused_cypher_writes_are_logged_without_text_or_values(self):
        private_value = "cypher-log-private-value-551907"

        def refusals():
            with open(os.path.join(self.directory, "server.stderr"), encoding="utf-8") as captured:
                output = captured.read()
            self.assertNotIn(private_value, output)
            return [line for line in output.splitlines() if "event=bolt_write_refused " in line]

        for user, query, reason in [
            # Needs `read` as well as `write`: refused when the statement is admitted.
            ("write-only", "MATCH (n {kala_handle:'greg'}) SET n.note = $v", "privilege"),
            ("writer", "MATCH (n {kala_handle:'greg'}) SET n.kala_handle = $v", "statement"),
        ]:
            with self.subTest(reason=reason):
                before = len(refusals())
                with self.driver(user).session(database="varrow") as session:
                    with self.assertRaises(ClientError):
                        session.run(query, v=private_value).consume()
                new = refusals()[before:]
                self.assertTrue(any(f"reason={reason}" in line for line in new), new)
                for line in new:
                    for key in ["connection_id=", "db=", "user=", "reason="]:
                        self.assertIn(key, line)
                    self.assertNotIn("MATCH", line)
                    self.assertNotIn("kala_handle", line)

    def test_cypher_correction_hint_can_be_used_on_the_same_connection(self):
        with self.driver().session(database="annex") as session:
            result = session.run("CREATE (:CHARACTER {kala_handle:'cypher_correction', "
                                 "name:'Original'}) AT TICK 8400")
            entry, = result.consume().metadata["kala"]["entries"]
            match = "MATCH (n {kala_handle:'cypher_correction'})"
            change = match + " SET n.name = 'Revised' AT TICK 8400"
            with self.assertRaises(ClientError) as error:
                session.run(change).consume()
            self.assertIn(f"entry#{entry}", error.exception.message)
            self.assertIn(f"[:correct {entry}", error.exception.message)
            session.run(PREFIX + f'[:correct {entry} :replace :value "Revised"]').consume()
            self.assertEqual(session.run(match + " RETURN n.name").single()[0], "Revised")

    def test_cypher_driver_forbidden_for_write_only_match_and_system(self):
        for user, db, query in [
            ("write-only", "varrow", "MATCH (n {kala_handle:'greg'}) SET n.x = 1"),
            ("root", "system", "CREATE (:ACCOUNT)"),
        ]:
            with self.subTest(user=user, database=db), self.driver(user).session(database=db) as session:
                with self.assertRaises(ClientError) as error:
                    session.execute_write(lambda tx: tx.run(query).consume())
                self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
                if user == "write-only":
                    self.assertIn("read", error.exception.message)

    def test_autocommit_records_summary_and_second_connection_visibility(self):
        with self.driver().session(database="varrow") as session:
            result = session.run(PREFIX + '[:assert :greg :alias "Driver auto commit" :at 300]')
            self.assertEqual(result.keys(), ["form", "op", "kind", "tick", "changed", "entries", "entity", "created"])
            row = dict(result.single())
            self.assertEqual({k: row[k] for k in ["form", "op", "kind", "tick", "changed"]},
                             {"form": 1, "op": 1, "kind": "assert", "tick": 300, "changed": 1})
            self.assertEqual(len(row["entries"]), 1)
            self.assertIs(type(row["entries"][0]), int)
            self.assertIsNone(row["entity"])
            self.assertIsNone(row["created"])
            summary = result.consume()
            self.assertEqual(summary.query_type, "w")
            self.assertEqual(summary.database, "varrow")
            self.assertEqual(summary.counters.properties_set, 1)
            self.assertTrue(summary.counters.contains_updates)
            # Unknown server keys are available on the public ResultSummary.
            self.assertEqual(summary.metadata["kala"]["changed"], 1)
            self.assertEqual(summary.metadata["kala"]["entries"], row["entries"])
            bookmark = "kala:varrow:" + str(summary.metadata["kala"]["tx"])
            self.assertEqual(summary.metadata["bookmark"], bookmark)
            self.assertEqual(session.last_bookmarks().raw_values, frozenset([bookmark]))
        self.assertIn("Driver auto commit", self.aliases(self.driver()))
        doctor = subprocess.run(
            [os.environ["KALA_BINARY"], "doctor", os.path.join(self.directory, "varrow.kala")],
            capture_output=True, text=True, timeout=15,
        )
        self.assertEqual(doctor.returncode, 0, doctor.stderr)

    def test_execute_write_execute_read_and_execute_query_direct_and_routed(self):
        for routed in [False, True]:
            with self.subTest(routing=routed):
                driver = self.driver(routing=routed)
                alias = "Driver managed routed" if routed else "Driver managed direct"
                with driver.session(database="varrow") as session:
                    rows = session.execute_write(lambda tx: list(tx.run(PREFIX + f'[:assert :greg :alias "{alias}" :at 301]')))
                    self.assertEqual(rows[0]["changed"], 1)
                    # IDs are assigned at COMMIT, after these records were pulled.
                    self.assertEqual(rows[0]["entries"], [])
                    aliases = session.execute_read(lambda tx: [r[0] for r in tx.run(PREFIX + "[:find ?a :where [:greg :alias ?a]]")])
                    self.assertIn(alias, aliases)
                records, _, keys = driver.execute_query(
                    PREFIX + f'[:assert :greg :alias "{alias} execute query" :at 302]',
                    database_="varrow",
                )
                self.assertIn("changed", keys)
                self.assertEqual(records[0]["changed"], 1)

    def test_explicit_commit_rollback_and_snapshot_reads(self):
        driver = self.driver()
        with driver.session(database="varrow") as session:
            with session.begin_transaction() as tx:
                created = tx.run(PREFIX + "[:create :CHARACTER :driver_commit :if-missing]").single()
                self.assertIsInstance(created["entity"], Node)
                self.assertTrue(created["created"])
                self.assertEqual(created["entries"], [])
                tx.run(PREFIX + '[:assert :driver_commit :name "Driver committed" :at -300]').consume()
                # Query reads the BEGIN snapshot, unlike the write applier.
                self.assertEqual(list(tx.run("MATCH (n {kala_handle: 'driver_commit'}) RETURN n")), [])
                tx.commit()
            self.assertTrue(session.last_bookmarks().raw_values)
            self.assertEqual(session.run("MATCH (n {kala_handle: 'driver_commit'}) RETURN n.name").single()[0], "Driver committed")
            with session.begin_transaction() as tx:
                tx.run(PREFIX + "[:create :CHARACTER :driver_rollback]").consume()
                tx.run(PREFIX + '[:assert :driver_rollback :name "Driver rolled back" :at -300]').consume()
                tx.rollback()
            self.assertEqual(list(session.run("MATCH (n {kala_handle: 'driver_rollback'}) RETURN n")), [])

    def test_bookmark_causal_read_across_connections(self):
        with self.driver().session(database="varrow") as first:
            first.execute_write(lambda tx: tx.run(PREFIX + '[:assert :greg :alias "Driver causal" :at 303]').consume())
            bookmarks = first.last_bookmarks()
        self.assertEqual(len(bookmarks.raw_values), 1)
        with self.driver().session(database="varrow", bookmarks=bookmarks) as second:
            aliases = second.execute_read(lambda tx: [r[0] for r in tx.run(PREFIX + "[:find ?a :where [:greg :alias ?a]]")])
            self.assertIn("Driver causal", aliases)
        with self.driver().session(database="varrow", bookmarks=neo4j.Bookmarks.from_raw_values(["not-a-kala-bookmark"])) as session:
            with self.assertRaises(ClientError) as error:
                session.run(ALLIES).consume()
            self.assertEqual(error.exception.code, "Neo.ClientError.Transaction.InvalidBookmark")
        # A bookmark of another database says nothing about this one: ignored.
        with self.driver().session(database="varrow", bookmarks=neo4j.Bookmarks.from_raw_values(["kala:another_database:999999"])) as session:
            self.assertEqual(len(list(session.run(ALLIES))), 2)

    def test_execute_query_across_two_databases_with_the_default_bookmark_manager(self):
        # The driver's default bookmark manager sends every bookmark it holds
        # with every request, whatever the target database.
        driver = self.driver()
        for round in range(2):
            for database in ["varrow", "annex"]:
                with self.subTest(database=database, round=round):
                    records, summary, _ = driver.execute_query(
                        PREFIX + f'[:assert :greg :alias "Two databases {round}" :at 310]',
                        database_=database,
                    )
                    self.assertEqual(records[0]["changed"], 1)
                    self.assertEqual(summary.database, database)
                    records, _, _ = driver.execute_query(
                        PREFIX + "[:find ?a :at 310 :where [:greg :alias ?a]]",
                        database_=database, routing_=neo4j.RoutingControl.READ,
                    )
                    self.assertIn(f"Two databases {round}", [r[0] for r in records])

    def test_default_routing_reads_never_take_a_write_slot(self):
        # execute_query begins a write-mode transaction unless told otherwise.
        # The server caps open WRITE transactions at 8; a transaction that
        # only reads must not count, however many run.
        driver = self.driver()
        held = []
        with contextlib.ExitStack() as stack:
            for _ in range(10):
                session = stack.enter_context(driver.session(database="varrow"))
                tx = stack.enter_context(session.begin_transaction())
                self.assertEqual(len(list(tx.run(ALLIES))), 2)
                held.append(tx)
            for _ in range(20):
                records, summary, _ = driver.execute_query(ALLIES, database_="varrow")
                self.assertEqual(len(records), 2)
                self.assertEqual(summary.query_type, "r")
            for tx in held:
                tx.commit()

    def test_admin_lists_databases_on_system_with_default_routing(self):
        records, _, _ = self.driver("root").execute_query("SHOW DATABASES", database_="system")
        self.assertLessEqual({"varrow", "annex", "system"}, {r["name"] for r in records})

    def test_conflicting_execute_write_is_retried_and_both_increments_commit(self):
        driver = self.driver()
        with driver.session(database="varrow") as session:
            session.run(PREFIX + '[:assert :greg :driver_counter 0 :at 5000]').consume()
        barrier = threading.Barrier(2, timeout=10)
        attempts = [0, 0]

        def increment(worker):
            with driver.session(database="varrow") as session:
                def work(tx):
                    attempts[worker] += 1
                    current = tx.run(PREFIX + "[:find ?n :where [:greg :driver_counter ?n]]").single()[0]
                    if attempts[worker] == 1:
                        barrier.wait()
                    tx.run(PREFIX + f"[:set :greg :driver_counter {current + 1} :at {5001 + current}]").consume()
                    return current + 1
                return session.execute_write(work)

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            futures = [pool.submit(increment, worker) for worker in range(2)]
            self.assertEqual(sorted(f.result(timeout=20) for f in futures), [1, 2])
        self.assertGreaterEqual(sum(attempts), 3)
        with driver.session(database="varrow") as session:
            self.assertEqual(session.run(PREFIX + "[:find ?n :where [:greg :driver_counter ?n]]").single()[0], 2)

    def test_notifications_inferred_schema_and_overlap(self):
        with self.driver().session(database="varrow") as session:
            inferred = session.run(PREFIX + "[:assert :greg :driver_inferred_age 17 :at 5100]").consume()
            self.assertEqual(inferred.metadata["kala"]["declared"], 1)
            self.assertEqual(inferred.counters.properties_set, 1)
            declarations = inferred.notifications or []
            self.assertTrue(any("inferred" in n["description"] for n in declarations), declarations)
            overlap = session.run(PREFIX + '[:assert :greg :name "Driver overlap" :at 5101]').consume()
            notifications = overlap.notifications or []
            self.assertTrue(any(n["code"] == "Kala.Warning.Overlap" for n in notifications), notifications)
            for note in declarations + notifications:
                self.assertTrue(note["code"].startswith("Kala."))
                self.assertTrue({"code", "title", "description", "severity"}.issubset(note))
                self.assertIn(note["severity"], ["WARNING", "INFORMATION"])

    def test_created_entities_edges_and_schema_have_honest_counters(self):
        with self.driver().session(database="varrow") as session:
            result = session.run(PREFIX + '[[:create :CHARACTER :driver_stats :if-missing] '
                                 '[:assert :driver_stats :name "Driver Stats" :at 5400] '
                                 '[:assert :driver_stats :located_in :varrow :at 5400]]')
            rows = list(result)
            self.assertEqual(len(rows), 3)
            self.assertIsInstance(rows[0]["entity"], Node)
            self.assertTrue(rows[0]["created"])
            summary = result.consume()
            self.assertEqual(summary.counters.nodes_created, 1)
            self.assertEqual(summary.counters.relationships_created, 1)
            self.assertEqual(summary.counters.properties_set, 1)
            schema = session.run(PREFIX + "[:attribute :driver_declared_age :i64]").consume()
            self.assertEqual(schema.query_type, "s")
            self.assertTrue(schema.counters.contains_updates)
            self.assertEqual(schema.metadata["kala"]["declared"], 1)

    def test_auto_commit_multiple_forms_report_partial_success(self):
        with self.driver().session(database="varrow") as session:
            with self.assertRaises(ClientError) as error:
                session.run(PREFIX + '[:assert :greg :alias "Driver first form" :at 5500]\n'
                            '[:assert :missing_driver_handle :alias "Must fail" :at 5500]').consume()
            self.assertIn("form 2 failed; 1 prior forms committed", error.exception.message)
            self.assertIn("Driver first form", [r[0] for r in session.run(PREFIX + "[:find ?a :where [:greg :alias ?a]]")])
            with self.assertRaises(ClientError) as error:
                session.run(PREFIX + '[:assert :greg :alias "Parameters refused" :at 5500]', unexpected=1).consume()
            self.assertEqual(error.exception.code, "Neo.ClientError.Request.Invalid")

    def test_read_and_write_privileges_and_read_transaction_mode(self):
        for routing in [False, True]:
            with self.subTest(routing=routing):
                with self.driver("reader", routing=routing).session(database="varrow") as session:
                    with self.assertRaises(ClientError) as error:
                        session.run(PREFIX + '[:assert :greg :alias "Must not commit" :at 5200]').consume()
                    self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
                with self.driver(routing=routing).session(database="varrow", default_access_mode=neo4j.READ_ACCESS) as session:
                    with self.assertRaises(ClientError) as error:
                        with session.begin_transaction() as tx:
                            tx.run(PREFIX + '[:assert :greg :alias "Read transaction" :at 5200]').consume()
                    self.assertEqual(error.exception.code, "Neo.ClientError.Transaction.ForbiddenDueToTransactionType")
        forms = [
            '[:set :greg :name "Ned"]',
            '[:create :CHARACTER :greg :if-missing]',
            '[:correct 67 :delete]',
            '[:migrate :name :unindexed]',
            '[[:assert :greg :alias "Hidden write"] [:set :greg :name "Ned"]]',
        ]
        for form in forms:
            with self.subTest(form=form):
                with self.driver("write-only").session(database="varrow") as session:
                    with self.assertRaises(ClientError) as error:
                        session.run(PREFIX + form).consume()
                    self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
                    # Migrations (like renames, deprecations and settings) need admin.
                    self.assertIn("admin" if "migrate" in form else "read", error.exception.message)
        self.assertNotIn("Hidden write", self.aliases(self.driver()))

    def test_write_only_reduced_report_and_failure_hide_existing_values(self):
        driver = self.driver("write-only", routing=True)
        with driver.session(database="varrow") as session:
            summary = session.run(PREFIX + '[:assert :greg :name "Driver private overlap" :at 5300]').consume()
            self.assertEqual(summary.metadata["kala"]["changed"], 1)
            for notification in summary.notifications or []:
                self.assertNotIn("Driver overlap", notification["description"])
                self.assertNotIn("Greg", notification["description"])
            with self.assertRaises(ClientError) as error:
                session.run(PREFIX + '[:retract :greg :name "Driver private overlap" :at 5300]').consume()
            self.assertIn("read", error.exception.message)
            self.assertNotIn("Driver private overlap", error.exception.message)
            self.assertNotIn("Greg", error.exception.message)

    def test_write_logs_are_structured_and_omit_form_values(self):
        private_value = "driver-log-private-value-734829"
        with self.driver().session(database="varrow") as session:
            session.run(PREFIX + f'[:assert :greg :alias "{private_value}" :at 5600]').consume()
        with self.driver("reader").session(database="varrow") as session:
            with self.assertRaises(ClientError):
                session.run(PREFIX + f'[:assert :greg :alias "{private_value}" :at 5601]').consume()
        with open(os.path.join(self.directory, "server.stderr"), encoding="utf-8") as captured:
            output = captured.read()
        commits = [line for line in output.splitlines() if "event=bolt_write_committed " in line]
        refusals = [line for line in output.splitlines() if "event=bolt_write_refused " in line]
        self.assertTrue(commits, "missing committed-write event")
        self.assertTrue(refusals, "missing refused-write event")
        self.assertTrue(any(" db=varrow " in line for line in commits))
        for line in commits:
            # The write server has two databases (varrow and annex).
            for key in ["connection_id=", " db=", "user=", "tx=", "forms=", "changed="]:
                self.assertIn(key, line)
        for line in refusals:
            for key in ["connection_id=", "db=", "user=", "reason="]:
                self.assertIn(key, line)
        self.assertNotIn(private_value, output)
        self.assertFalse(any(":greg" in line or "[:assert" in line for line in commits + refusals))

    def test_system_is_never_writable_and_file_stays_identical(self):
        path = os.path.join(self.directory, "system.kala")
        with open(path, "rb") as stream:
            before = stream.read()
        for user in ["root", "writer", "write-only"]:
            for explicit in [False, True]:
                with self.subTest(user=user, explicit=explicit):
                    with self.driver(user).session(database="system") as session:
                        with self.assertRaises(ClientError) as error:
                            if explicit:
                                with session.begin_transaction() as tx:
                                    tx.run(PREFIX + "[:create :ACCOUNT :driver_intruder]").consume()
                            else:
                                session.run(PREFIX + "[:create :ACCOUNT :driver_intruder]").consume()
                        self.assertEqual(error.exception.code, "Neo.ClientError.Security.Forbidden")
        with open(path, "rb") as stream:
            self.assertEqual(stream.read(), before)


if __name__ == "__main__":
    print("Official Python driver: neo4j==" + neo4j.__version__, flush=True)
    unittest.main(verbosity=2)
