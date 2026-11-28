// The shell harness owns the server, accounts, Varrow fixture and test CA.
package driver_test

import (
	"context"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/neo4j/neo4j-go-driver/v5/neo4j"
)

const datalog = "// kala:datalog\n"

func testContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func openDriver(t *testing.T, ctx context.Context, uri string, auth neo4j.AuthToken, configure ...func(*neo4j.Config)) neo4j.DriverWithContext {
	t.Helper()
	driver, err := neo4j.NewDriverWithContext(uri, auth, configure...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { driver.Close(context.Background()) })
	if err := driver.VerifyConnectivity(ctx); err != nil {
		t.Fatal(err)
	}
	return driver
}

func session(t *testing.T, ctx context.Context, driver neo4j.DriverWithContext, database string) neo4j.SessionWithContext {
	t.Helper()
	s := driver.NewSession(ctx, neo4j.SessionConfig{DatabaseName: database})
	t.Cleanup(func() { s.Close(context.Background()) })
	return s
}

func records(ctx context.Context, s neo4j.SessionWithContext, query string) ([]*neo4j.Record, error) {
	result, err := s.Run(ctx, query, nil)
	if err != nil {
		return nil, err
	}
	return result.Collect(ctx)
}

func mustQuery(t *testing.T, ctx context.Context, s neo4j.SessionWithContext, query string) []*neo4j.Record {
	t.Helper()
	rows, err := records(ctx, s, query)
	if err != nil {
		t.Fatal(err)
	}
	return rows
}

func kala(t *testing.T, ctx context.Context, stdin string, args ...string) []byte {
	t.Helper()
	cmd := exec.CommandContext(ctx, os.Getenv("KALA_BINARY"), args...)
	cmd.Stdin = strings.NewReader(stdin)
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("kala %s: %v", args[0], err)
	}
	return out
}

func TestAccountGrants(t *testing.T) {
	ctx := testContext(t)
	dir := filepath.Dir(os.Getenv("KALA_WORLD"))
	// Supply a per-run credential through stdin on the loopback fixture.
	var random [24]byte
	if _, err := rand.Read(random[:]); err != nil {
		t.Fatal(err)
	}
	password := hex.EncodeToString(random[:])
	kala(t, ctx, password+"\n", "user", "add", dir, "go-reader@localhost", "--password-stdin")
	kala(t, ctx, "", "user", "grant", dir, "go-reader@localhost", "read", "varrow")
	for _, account := range []struct{ user, password string }{{"test-user", ""}, {"go-reader", password}} {
		t.Run(account.user, func(t *testing.T) {
			driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.BasicAuth(account.user, account.password, ""))
			query := "MATCH (n {kala_handle:'varrow'}) RETURN n.name"
			rows := mustQuery(t, ctx, session(t, ctx, driver, "varrow"), query)
			if len(rows) != 1 || rows[0].Values[0] != "Varrow" {
				t.Fatalf("unexpected Varrow result: %v", rows)
			}
			_, err := records(ctx, session(t, ctx, driver, "hidden"), query)
			var refusal *neo4j.Neo4jError
			if !errors.As(err, &refusal) || refusal.Code != "Neo.ClientError.Security.Forbidden" {
				t.Fatalf("expected forbidden database, got %v", err)
			}
		})
	}
}

func TestEdwinsAllies(t *testing.T) {
	ctx := testContext(t)
	driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.NoAuth())
	s := session(t, ctx, driver, "varrow")
	const match = "MATCH (e {kala_handle:'edwin'})-[:ally_of]->(a) AT TICK "
	for _, tc := range []struct {
		name, lang, query string
		handles           []string
	}{
		{"datalog", "datalog", "[:find ?a :at -210 :where [:edwin :ally_of ?a]]", []string{"greg", "tomas"}},
		{"cypher", "cypher", match + "-210 RETURN DISTINCT a", []string{"greg", "tomas"}},
		// Before the retcon, Greg was still Edwin's ally at -195.
		{"historical", "cypher", match + "-195 CANON 10 RETURN DISTINCT a", []string{"greg", "mara", "tomas"}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			query := tc.query
			if tc.lang == "datalog" {
				query = datalog + query
			}
			var handles []string
			for _, record := range mustQuery(t, ctx, s, query) {
				node := record.Values[0].(neo4j.Node)
				handles = append(handles, node.Props["kala_handle"].(string))
			}
			sort.Strings(handles)
			var cli struct {
				Rows [][]struct{ Value struct{ Handle string } }
			}
			if err := json.Unmarshal(kala(t, ctx, "", "query", os.Getenv("KALA_WORLD"), "--json", "--lang", tc.lang, tc.query), &cli); err != nil {
				t.Fatal(err)
			}
			var cliHandles []string
			for _, row := range cli.Rows {
				cliHandles = append(cliHandles, row[0].Value.Handle)
			}
			sort.Strings(cliHandles)
			if !reflect.DeepEqual(handles, cliHandles) || !reflect.DeepEqual(handles, tc.handles) {
				t.Fatalf("allies: Go %v, CLI %v, want %v", handles, cliHandles, tc.handles)
			}
		})
	}
}

func TestGraphTypes(t *testing.T) {
	ctx := testContext(t)
	driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.NoAuth())
	rows := mustQuery(t, ctx, session(t, ctx, driver, "varrow"),
		"MATCH p = (e {kala_handle:'edwin'})-[r:present]->(c) AT TICK -240 RETURN e, r, c, p")
	if len(rows) != 1 {
		t.Fatalf("got %d paths", len(rows))
	}
	values := rows[0].Values
	start, rel, end, path := values[0].(neo4j.Node), values[1].(neo4j.Relationship), values[2].(neo4j.Node), values[3].(neo4j.Path)
	if start.Props["kala_handle"] != "edwin" || end.Props["kala_handle"] != "carnath" ||
		rel.Type != "present" || rel.Props["disguised"] != true ||
		rel.StartElementId != start.ElementId || rel.EndElementId != end.ElementId {
		t.Fatalf("unexpected edge %v between %v and %v", rel, start, end)
	}
	if !reflect.DeepEqual(path.Nodes, []neo4j.Node{start, end}) || !reflect.DeepEqual(path.Relationships, []neo4j.Relationship{rel}) {
		t.Fatalf("unexpected path: %v", path)
	}
}

type transaction interface {
	Run(context.Context, string, map[string]any) (neo4j.ResultWithContext, error)
}

func write(ctx context.Context, tx transaction, query string, params map[string]any) error {
	res, err := tx.Run(ctx, query, params)
	if err != nil {
		return err
	}
	_, err = res.Consume(ctx)
	return err
}

func TestTransactions(t *testing.T) {
	ctx := testContext(t)
	driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.NoAuth())
	s := session(t, ctx, driver, "varrow")
	// The second statement resolves the handle made by the first one.
	work := func(tx transaction, handle string) error {
		if err := write(ctx, tx, datalog+"[:create :CHARACTER :"+handle+"]", nil); err != nil {
			return err
		}
		return write(ctx, tx, datalog+"[:assert :"+handle+" :name \"Scout\" :at 7000]", nil)
	}
	_, err := s.ExecuteWrite(ctx, func(tx neo4j.ManagedTransaction) (any, error) {
		return nil, work(tx, "go_commit")
	})
	if err != nil {
		t.Fatal(err)
	}
	tx, err := s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Close(ctx)
	if err := work(tx, "go_rollback"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Rollback(ctx); err != nil {
		t.Fatal(err)
	}
	committed := mustQuery(t, ctx, s, "MATCH (n {kala_handle:'go_commit'}) RETURN n.name")
	if len(committed) != 1 || committed[0].Values[0] != "Scout" {
		t.Fatalf("missing committed write: %v", committed)
	}
	if rows := mustQuery(t, ctx, s, "MATCH (n {kala_handle:'go_rollback'}) RETURN n"); len(rows) != 0 {
		t.Fatalf("rolled back handle was visible: %v", rows)
	}
}

func TestManagedConflictRetry(t *testing.T) {
	ctx := testContext(t)
	driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.NoAuth())
	s := session(t, ctx, driver, "varrow")
	other := session(t, ctx, driver, "varrow")
	mustQuery(t, ctx, s, "CREATE (:CHARACTER {kala_handle:'go_counter', points:0}) AT TICK 7100")
	const match = "MATCH (n {kala_handle:'go_counter'})"
	const read = match + " RETURN n.points"
	attempts := 0
	_, err := s.ExecuteWrite(ctx, func(tx neo4j.ManagedTransaction) (any, error) {
		attempts++
		res, err := tx.Run(ctx, read, nil)
		if err != nil {
			return nil, err
		}
		record, err := res.Single(ctx)
		if err != nil {
			return nil, err
		}
		points := record.Values[0].(int64)
		if attempts == 1 {
			// A separate connection commits after BEGIN. The managed
			// transaction's read is stale, so its first COMMIT must conflict.
			if _, err := records(ctx, other, match+" SET n.points = 1 AT TICK 7101"); err != nil {
				return nil, err
			}
		}
		err = write(ctx, tx, match+" SET n.points = $points AT TICK $tick", map[string]any{
			"points": points + 1, "tick": 7101 + points,
		})
		return points + 1, err
	})
	if err != nil {
		t.Fatal(err)
	}
	rows := mustQuery(t, ctx, s, read)
	if attempts != 2 || len(rows) != 1 || rows[0].Values[0] != int64(2) {
		t.Fatalf("got %d attempts and result %v", attempts, rows)
	}
}

func TestDiscovery(t *testing.T) {
	ctx := testContext(t)
	driver := openDriver(t, ctx, os.Getenv("KALA_BOLT_URI"), neo4j.NoAuth())
	const db = "go_campaign"
	world := filepath.Join(filepath.Dir(os.Getenv("KALA_WORLD")), db+".kala")
	kala(t, ctx, "", "create", world)
	kala(t, ctx, "[[:node-type :NPC] [:create :NPC :ren] [:assert :ren :name \"Ren\"]]\n", "transact", world, "-")
	s := session(t, ctx, driver, db)
	// Another session may have just used the shared directory rescan.
	deadline := time.Now().Add(5 * time.Second)
	for {
		rows, err := records(ctx, s, "MATCH (n {kala_handle:'ren'}) RETURN n.name")
		if err == nil {
			if len(rows) != 1 || rows[0].Values[0] != "Ren" {
				t.Fatalf("query selected the wrong world: %v", rows)
			}
			break
		}
		var failure *neo4j.Neo4jError
		if !errors.As(err, &failure) || failure.Code != "Neo.ClientError.Database.DatabaseNotFound" || time.Now().After(deadline) {
			t.Fatalf("querying newly created world: %v", err)
		}
		time.Sleep(100 * time.Millisecond)
	}
	names := mustQuery(t, ctx, session(t, ctx, driver, "system"), "SHOW DATABASES YIELD name")
	for _, record := range names {
		if record.Values[0] == db {
			return
		}
	}
	t.Fatalf("SHOW DATABASES omitted %s: %v", db, names)
}

func TestVerifiedTLS(t *testing.T) {
	uri := os.Getenv("KALA_GO_TLS_URI")
	if uri == "" {
		t.Skip("OpenSSL test certificates unavailable")
	}
	ctx := testContext(t)
	ca, err := os.ReadFile(filepath.Join(os.Getenv("KALA_TLS_FIXTURES"), "ca.pem"))
	if err != nil {
		t.Fatal(err)
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(ca) {
		t.Fatal("could not read test CA")
	}
	driver := openDriver(t, ctx, uri, neo4j.NoAuth(), func(config *neo4j.Config) {
		config.TlsConfig = &tls.Config{RootCAs: pool}
	})
	rows := mustQuery(t, ctx, session(t, ctx, driver, "varrow"), "MATCH (n {kala_handle:'varrow'}) RETURN n.name")
	if len(rows) != 1 || rows[0].Values[0] != "Varrow" {
		t.Fatalf("unexpected TLS query result: %v", rows)
	}
}
