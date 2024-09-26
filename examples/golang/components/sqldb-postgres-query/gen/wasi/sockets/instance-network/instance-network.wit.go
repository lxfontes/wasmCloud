// Code generated by wit-bindgen-go. DO NOT EDIT.

// Package instancenetwork represents the imported interface "wasi:sockets/instance-network@0.2.0".
//
// This interface provides a value-export of the default network handle..
package instancenetwork

import (
	"github.com/bytecodealliance/wasm-tools-go/cm"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/sqldb-postgres-query/gen/wasi/sockets/network"
)

// InstanceNetwork represents the imported function "instance-network".
//
// Get a handle to the default network.
//
//	instance-network: func() -> network
//
//go:nosplit
func InstanceNetwork() (result network.Network) {
	result0 := wasmimport_InstanceNetwork()
	result = cm.Reinterpret[network.Network]((uint32)(result0))
	return
}

//go:wasmimport wasi:sockets/instance-network@0.2.0 instance-network
//go:noescape
func wasmimport_InstanceNetwork() (result0 uint32)
