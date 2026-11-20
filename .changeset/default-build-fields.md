---
default: patch
---

# A Build written with zeabur-builder-api leaves out default triggers and concurrency

A client no longer writes `triggers: {}` and `concurrency: Replace` when it sets neither, so it does not own those fields under server-side apply. The API server still fills in both defaults.
