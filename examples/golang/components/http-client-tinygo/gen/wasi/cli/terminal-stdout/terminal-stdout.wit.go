// Code generated by wit-bindgen-go. DO NOT EDIT.

// Package terminalstdout represents the imported interface "wasi:cli/terminal-stdout@0.2.0".
//
// An interface providing an optional `terminal-output` for stdout as a
// link-time authority.
package terminalstdout

import (
	"github.com/bytecodealliance/wasm-tools-go/cm"
	terminaloutput "github.com/wasmcloud/wasmcloud/examples/golang/components/http-client-tinygo/gen/wasi/cli/terminal-output"
)

// GetTerminalStdout represents the imported function "get-terminal-stdout".
//
// If stdout is connected to a terminal, return a `terminal-output` handle
// allowing further interaction with it.
//
//	get-terminal-stdout: func() -> option<terminal-output>
//
//go:nosplit
func GetTerminalStdout() (result cm.Option[terminaloutput.TerminalOutput]) {
	wasmimport_GetTerminalStdout(&result)
	return
}

//go:wasmimport wasi:cli/terminal-stdout@0.2.0 get-terminal-stdout
//go:noescape
func wasmimport_GetTerminalStdout(result *cm.Option[terminaloutput.TerminalOutput])
