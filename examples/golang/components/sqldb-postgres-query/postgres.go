package main

import (
	"fmt"

	"github.com/bytecodealliance/wasm-tools-go/cm"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/sqldb-postgres-query/gen/wasmcloud/examples/invoke"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/sqldb-postgres-query/gen/wasmcloud/postgres/query"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/sqldb-postgres-query/gen/wasmcloud/postgres/types"
)

const CREATE_TABLE_QUERY = `
CREATE TABLE IF NOT EXISTS example (
	id BIGINT GENERATED BY DEFAULT AS IDENTITY,
	description text NOT NULL,
	created_at timestamptz NOT NULL DEFAULT NOW()
)
`

const SAMPLE_IDENTIFIER = "inserted example go row!"

// A basic insert query, using Postgres `RETURNING` syntax,
// which returns the contents of the row that was inserted
const INSERT_QUERY = `INSERT INTO example (description) VALUES ($1) RETURNING *`

const SELECT_QUERY = `SELECT id,created_at FROM example WHERE description = $1 ORDER BY created_at DESC LIMIT 1`

// type aliases for more readable code
type (
	PgValue  = types.PgValue
	RowEntry = types.ResultRowEntry
)

func init() {
	invoke.Exports.Call = componentCall
}

func componentCall() string {
	query := Query(CREATE_TABLE_QUERY)
	if query.IsErr() {
		return fmt.Sprintf("ERROR: failed to create table: %v", query.Err())
	}

	insertResult := Query(INSERT_QUERY, types.PgValueText(SAMPLE_IDENTIFIER))
	if insertResult.IsErr() {
		return fmt.Sprintf("ERROR: failed to insert row: %v", insertResult.Err())
	}

	selectResult := Query(SELECT_QUERY, types.PgValueText(SAMPLE_IDENTIFIER))
	if selectResult.IsErr() {
		return fmt.Sprintf("ERROR: failed to select rows: %v", selectResult.Err())
	}
	selectedRows := selectResult.OK().Slice()
	if len(selectedRows) > 0 {
		firstRow := selectedRows[0].Slice()
		rowId := firstRow[0].Value.Int8()
		createdAt := firstRow[1].Value.TimestampTz()
		return fmt.Sprintf("SUCCESS: we selected a row!\nID: %d, Created At: %s", *rowId, formatTimestamp(createdAt.Timestamp))
	}
	return "ERROR: failed to retrieve inserted row"
}

func formatTimestamp(ts types.Timestamp) string {
	tsDate := ts.Date.Ymd()
	tsTime := ts.Time

	return fmt.Sprintf("%02d-%02d-%02dT%02d:%02d:%02dZ", tsDate.F0, tsDate.F1, tsDate.F2, tsTime.Hour, tsTime.Min, tsTime.Sec)
}

// Helper function to assist with readability in the example when querying the database
func Query(stmt string, params ...PgValue) cm.Result[query.QueryErrorShape, cm.List[types.ResultRow], types.QueryError] {
	return query.Query(stmt, cm.ToList(params))
}

//go:generate go run github.com/bytecodealliance/wasm-tools-go/cmd/wit-bindgen-go generate --world component --out gen ./wit
func main() {}
