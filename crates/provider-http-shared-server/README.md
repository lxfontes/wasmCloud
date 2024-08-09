# wasmCloud HTTP Server Provider

This capability provider implements the `wasmcloud:httpserver` capability contract, and enables a component to accept incoming HTTP(s) requests. It is implemented in Rust with the [warp](https://docs.rs/warp/) web server framework and the fast and scalable [hyper](https://docs.rs/hyper/) http implementation.

For more information on the operations supported by this provider, please check out its corresponding [interface](https://github.com/wasmCloud/interfaces/blob/main/httpserver/httpserver.smithy).

To compile, run the following command at the git repository root:

```sh
wash build -p src/bin/http-shared-server-provider
```

## Configuration Settings

Configuration settings for the httpserver provider are described in [settings](./settings.md). Example:

```json
  components:
    - name: ingress
      type: capability
      properties:
        image: ghcr.io/wasmcloud/http-shared-server:0.22.0
        config:
          - name: provider_config
            properties:
              address: "0.0.0.0:8080"
```

## Link Settings

Components linking to the HTTP Server provider must provide a `hostname` in the `source_config`. Example:

```
spec:
  components:
    - name: first-component
      type: component
      properties:
        image: ghcr.io/wasmcloud/components/http-hello-world-rust:0.1.0
      traits:
        - type: spreadscaler
          properties:
            instances: 1

    - name: second-component
      type: component
      properties:
        image: ghcr.io/wasmcloud/components/http-hello-world-rust:0.1.0
      traits:
        - type: spreadscaler
          properties:
            instances: 1

    - name: shared-http-server
      type: capability
      properties:
        image: ghcr.io/wasmcloud/http-shared-server:0.22.0
        config:
          - name: provider_config
            properties:
              address: "0.0.0.0:9090"
      traits:
        - type: link
          properties:
            target: first-component
            namespace: wasi
            package: http
            interfaces: [incoming-handler]
            source_config:
              - name: first-component-http
                properties:
                  hostname: "first-component.internal"
        - type: link
          properties:
            target: second-component
            namespace: wasi
            package: http
            interfaces: [incoming-handler]
            source_config:
              - name: second-component-http
                properties:
                  hostname: "second-component.internal"

```

### ⚠️ Caution - Port Ownership

The key difference between this capability provider and `provider-http-server` port allocation.
This capability provider will instantiate a single http server on a single port, and route requests to components via hostname matching.
The `provider-http-server` capability provider will allocate a port per component, and each component will have its own server instance.

For more hands-on tutorials on building components, including HTTP server components,
see the [wasmcloud.dev](https://wasmcloud.dev) website.
