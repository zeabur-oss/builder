---
default: patch
---

# Upload Secrets carry the registry's CA

When the default registry has a CA of its own (`registry.ca` or `registry.caFrom`), each upload Secret holds it in `ca.crt`. An uploader that may read the Secret can now trust the registry without reading the registry's own Secret in another namespace.
