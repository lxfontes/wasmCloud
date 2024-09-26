package main

import (
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"strings"

	"github.com/bytecodealliance/wasm-tools-go/cm"
	incominghandler "github.com/wasmcloud/wasmcloud/examples/golang/components/http-echo-tinygo/gen/wasi/http/incoming-handler"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/http-echo-tinygo/gen/wasi/http/types"
	"github.com/wasmcloud/wasmcloud/examples/golang/components/http-echo-tinygo/gen/wasi/logging/logging"
)

// A struct containing information about a request,
// sent back as a response JSON from the echo server
type EchoResponse struct {
	Method      string `json:"method"`
	Path        string `json:"path"`
	QueryString string `json:"queryString,omitempty"`
	Body        string `json:"body,omitempty"`
}

// Implementation of the `wasi-http:incoming-handler` export in the `echo` world (see `wit/echo.wit)`
//
// This method's signature and implementation use code generated by `wit-bindgen`, in the `gen` folder
// When building with `wash build`, `wit-bindgen` is run automatically to generate the classes that have been aliased above.
func echoHandler(request types.IncomingRequest, responseWriter types.ResponseOutparam) {
	var er EchoResponse

	// Create a list of keyvalue tuples usable as response headers
	responseHeaders := types.NewFields()
	responseHeaders.Append(types.FieldKey("Content-Type"), types.FieldValue(cm.ToList([]uint8("application/json"))))

	// Detect request method, use to build EchoResponse
	method := request.Method()
	if method.Connect() {
		er.Method = http.MethodConnect
	} else if method.Delete() {
		er.Method = http.MethodDelete
	} else if method.Get() {
		er.Method = http.MethodGet
	} else if method.Head() {
		er.Method = http.MethodHead
	} else if method.Options() {
		er.Method = http.MethodOptions
	} else if method.Patch() {
		er.Method = http.MethodPatch
	} else if method.Post() {
		er.Method = http.MethodPost
	} else if method.Put() {
		er.Method = http.MethodPut
	} else if method.Trace() {
		er.Method = http.MethodTrace
	} else if other := method.Other(); other != nil {
		er.Method = *other
	}

	// Retrieve the request path w/ query fragment (an echo.Option[T])
	var pathWithQuery string
	if rawPathWithQuery := request.PathWithQuery(); !rawPathWithQuery.None() {
		pathWithQuery = *rawPathWithQuery.Some()
	} else {

		writeHttpResponse(responseWriter, http.StatusInternalServerError, responseHeaders, []byte("{\"error\":\"missing path\"}"))
		return
	}

	// Split the path to retrieve the query element, building the EchoResponse object
	splitPathQuery := strings.Split(pathWithQuery, "?")
	er.Path = splitPathQuery[0]
	if len(splitPathQuery) > 1 {
		er.QueryString = splitPathQuery[1]
	}

	// Consume the request body
	maybeBody := request.Consume()
	if maybeBody.IsErr() {
		writeHttpResponse(responseWriter, http.StatusInternalServerError, responseHeaders, []byte("{\"error\":\"failed to read request body\"}"))
		return
	}
	body := maybeBody.OK()

	// Convert the request body to a stream
	maybeBodyStream := body.Stream()
	if maybeBodyStream.IsErr() {
		writeHttpResponse(responseWriter, http.StatusInternalServerError, responseHeaders, []byte("{\"error\":\"failed to convert body into stream\"}"))
		return
	}
	bodyStream := maybeBodyStream.OK()

	// Read the maximum amount of bytes possible from the stream
	maybeReadStream := bodyStream.BlockingRead(math.MaxInt64)
	if maybeReadStream.IsErr() {
		// If the body is empty, we'll get a closed error, in which case we *do not* want to throw an error.
		errKind := maybeReadStream.Err()
		if errKind.Closed() {
			// There was likely *no* data in the body (ex. a GET request)
			er.Body = ""
		} else {
			// if we received some other error, report it
			logging.Log(logging.LevelError, "failed to read incoming body stream", fmt.Sprintf("error kind [%v]"))
			writeHttpResponse(responseWriter, http.StatusInternalServerError, responseHeaders, []byte("{\"error\":\"failed to read incoming body stream\"}"))
			return
		}
	} else {
		// If reading from the request did not error, we can update the EchoResponse object with the request body
		er.Body = string(maybeReadStream.OK().Slice())
	}

	// Log information about the request
	logging.Log(logging.LevelInfo, "method", er.Method)
	logging.Log(logging.LevelInfo, "path", er.Path)
	logging.Log(logging.LevelInfo, "queryString", er.QueryString)
	logging.Log(logging.LevelInfo, "body", er.Body)

	// Marshal the EchoResponse object we've been building to JSON
	bBody, err := json.Marshal(er)
	if err != nil {
		writeHttpResponse(responseWriter, http.StatusInternalServerError, responseHeaders, []byte("{\"error\":\"failed to marshal response\"}"))
		return
	}

	writeHttpResponse(responseWriter, http.StatusOK, responseHeaders, bBody)
}

// Write an outgoing HTTP response (status, headers, body) to a given writer (in WIT terms a ResponseOutparam)
func writeHttpResponse(responseOutparam types.ResponseOutparam, statusCode uint16, headers types.Fields, body []byte) {
	logging.Log(logging.LevelInfo, "writeHttpResponse", "writing response: "+string(body))

	// Build the new HTTP outgoing response
	outgoingResponse := types.NewOutgoingResponse(headers)
	outgoingResponse.SetStatusCode(types.StatusCode(statusCode))

	// Retrieve the body inside the outgoing response
	maybeOutgoingBody := outgoingResponse.Body()
	if maybeOutgoingBody.IsErr() {
		logging.Log(logging.LevelError, "writeHttpResponse", "failed to acquire body")
		return
	}
	outgoingBody := maybeOutgoingBody.OK()

	// Create a writable stream for the response body content
	maybeOutgoingStream := outgoingBody.Write()
	if maybeOutgoingStream.IsErr() {
		logging.Log(logging.LevelError, "writeHttpResponse", "failed to acquire body stream")
		return
	}
	outgoingStream := maybeOutgoingStream.OK()

	// Write the body to the outgoing response
	res := outgoingStream.BlockingWriteAndFlush(cm.ToList(body))
	if res.IsErr() {
		return
	}

	outgoingStream.ResourceDrop()
	types.OutgoingBodyFinish(*outgoingBody, cm.None[types.Fields]())

	// Set the response on the outparam
	types.ResponseOutparamSet(responseOutparam, cm.OK[cm.Result[types.ErrorCodeShape, types.OutgoingResponse, types.ErrorCode]](outgoingResponse))
}

func init() {
	// NOTE: Set the `wasi-http:incoming-handler` export in the `echo` world to the `echoHandler` function
	incominghandler.Exports.Handle = echoHandler
}

// NOTE: The 'go generate' instruction below converts WIT worlds to Go code
//
//go:generate go run github.com/bytecodealliance/wasm-tools-go/cmd/wit-bindgen-go generate --world echo --out gen ./wit
func main() {}
