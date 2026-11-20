---
default: minor
---

# The build container reads a typed plan

`zeabur-builder-job build` reads the builds as JSON from a file, instead of `buildctl` command lines it parsed again. The file is the Pod's `builder.zeabur.com/build-plan` annotation, mounted with a downward API volume, which the admission policy now allows for that annotation only.

A ClusterBuilder's `frontend.opts` now takes names of at most 128 characters and values of at most 512, so the largest plan fits in the 256 KiB a Pod's annotations hold. Shorten longer options before the upgrade.
