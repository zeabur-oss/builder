---
default: minor
---

# Build logs are NDJSON events with step durations

The build container writes one JSON event per line: `started`, `log` (with `stream` `stdout` or `stderr`), `done` and `error` (with `seconds`), `cached`, and `message` for buildctl's own output. Every event carries its step name, and step numbers continue across Railpack's two `buildctl` runs. `kubectl logs` shows JSON; `jq -r '.text // .message // .name'` reads it as text. The `fetch` and `plan` steps keep their text output.
