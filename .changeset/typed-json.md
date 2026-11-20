---
default: patch
---

# Typed JSON everywhere

Webhook bodies, Railpack plans, registry replies and the objects the daemon writes are read and built through typed structs. A webhook whose repository fields are not strings is refused with 400, like a body that is not JSON. clippy refuses `serde_json::Value`, `DynamicObject` and their accessors outside the few places that pass through JSON of any shape.
