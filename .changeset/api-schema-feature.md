---
default: minor
---

# zeabur-builder-api builds without schemars when its schema feature is off

The default feature `schema` adds `JsonSchema` to every type and `definitions()`. A crate that only reads and writes the objects can set `default-features = false` and leaves out schemars, dyn-clone and ref-cast.
