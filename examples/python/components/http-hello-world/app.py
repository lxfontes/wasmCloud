from hello import exports
from hello.types import Ok
from hello.imports.types import (
    IncomingRequest, ResponseOutparam,
    OutgoingResponse, Fields, OutgoingBody
)

class IncomingHandler(exports.IncomingHandler):
    def handle(self, _: IncomingRequest, response_out: ResponseOutparam):
        # Construct the HTTP response to send back 
        outgoingResponse = OutgoingResponse(Fields.from_list([]))
        # Set the status code to OK
        outgoingResponse.set_status_code(200)
        # Write our Hello World message to the response body
        outgoingBody = outgoingResponse.body()
        ResponseOutparam.set(response_out, Ok(outgoingResponse))
        outgoingBody.write().blocking_write_and_flush(bytes("Hello from Python!\n", "utf-8"))
        OutgoingBody.finish(outgoingBody, None)