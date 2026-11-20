---
default: patch
---

# Typed webhook and Railpack plan parsing

Webhook bodies and Railpack plans are read through typed structs. A webhook whose repository fields are not strings is refused with 400, like a body that is not JSON. clippy refuses the `serde_json::Value` accessors.
