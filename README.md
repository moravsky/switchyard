# switchyard

An OpenAI-compatible model-routing gateway. Put one address in front of several
OpenAI-compatible model servers (vLLM, llama.cpp, MLX, Ollama, ...) and switchyard
forwards each request to the backend named by its `model`.

## Features

- **Routing by model.** It reads the top-level `"model"` of JSON bodies and the
  `model` field of `multipart/form-data` uploads (for example, audio
  transcriptions). Aliases are supported, and a default backend handles
  requests that carry no model.
- **Model rewriting.** `upstream_model` replaces the model in the forwarded
  body in place, with exact lengths, so the backend sees its own model name.
- **Model inventory.** `GET /v1/models` queries every backend's `/v1/models`
  with a 2-second deadline per backend and returns one entry per backend that
  answers, under its switchyard name. Unreachable backends are left out.
- **Streaming.** Responses, including server-sent events, are relayed as they
  arrive. A backend gets up to 15 minutes to start a response and between
  streamed events.
- **Strict HTTP/1.1.** Malformed or ambiguous requests, such as repeated
  `Content-Length`, `Transfer-Encoding` together with `Content-Length`, or
  malformed chunks, are rejected with 400 before anything is forwarded.
  Persistent connections and pipelined requests are supported.
- **Bounded memory.** Request bodies go to an unlinked temporary file and are
  scanned through a fixed-size window, so memory stays at a few MiB however
  large the upload is.
- **Credential confinement.** Client credentials go only to the selected
  backend and never to inventory queries. Connections go only to configured
  backend addresses, and redirects are passed through, not followed.
- **Consistent errors.** Gateway errors are JSON
  (`{"error": {"message", "type": "switchyard_error", "code"}}`) with fixed
  messages that never echo client input.

One thread per connection. The only dependencies are the standard library,
`memchr`, `serde_json` and `toml`.

## Configuration

```toml
listen = "127.0.0.1:8090"     # address to bind
default = "qwen"              # optional: backend for requests with no "model"
max_body = 268435456          # optional: max request body in bytes (default 256 MiB)

[[backend]]
name = "qwen"                     # model name clients send; listed in /v1/models
url  = "http://127.0.0.1:8081"    # backend base URL (http only; a path prefix is allowed)
aliases = ["qwen3-27b"]           # optional: other model names that route here
upstream_model = "mlx/Qwen3-27B"  # optional: model name sent to the backend
```

The config is TOML 1.0. Unknown fields, duplicate names or aliases, missing or
non-`http://` URLs, and a `default` that isn't a backend name are rejected at
startup with a message on stderr and a non-zero exit.

## Build and run

```sh
cargo build --release
./target/release/gateway switchyard.toml
```

For a static Linux binary:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Scratch files go to the system temp directory (`$TMPDIR`, default `/tmp`).

## Limitations

- Plain HTTP only, on both the client and backend sides (no TLS).
- HTTP/1.1 only.
- No authentication of its own. Run it inside your own network boundary.
- The config is read once at startup.

## License

MIT. See [LICENSE](LICENSE).

---

## Author

**Petr Moravsky** ([petr@structuredtrading.co](mailto:petr@structuredtrading.co)) — developer.

If switchyard is useful to you or served as a reference for your own development — [star the repo](https://github.com/moravsky/switchyard) and [leave a tip](https://ko-fi.com/moravsky).
