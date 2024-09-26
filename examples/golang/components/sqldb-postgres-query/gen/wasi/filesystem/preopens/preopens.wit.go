// Code generated by wit-bindgen-go. DO NOT EDIT.

// Package preopens represents the imported interface "wasi:filesystem/preopens@0.2.0".
package preopens

import (
	"github.com/bytecodealliance/wasm-tools-go/cm"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/sqldb-postgres-query/gen/wasi/filesystem/types"
)

// GetDirectories represents the imported function "get-directories".
//
// Return the set of preopened directories, and their path.
//
//	get-directories: func() -> list<tuple<descriptor, string>>
//
//go:nosplit
func GetDirectories() (result cm.List[cm.Tuple[types.Descriptor, string]]) {
	wasmimport_GetDirectories(&result)
	return
}

//go:wasmimport wasi:filesystem/preopens@0.2.0 get-directories
//go:noescape
func wasmimport_GetDirectories(result *cm.List[cm.Tuple[types.Descriptor, string]])
