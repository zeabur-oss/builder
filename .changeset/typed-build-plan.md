---
default: minor
---

# The build container reads a typed plan

`zeabur-builder-job build` reads the builds as JSON from a file, instead of `buildctl` command lines it parsed again. The file is the Pod's `builder.zeabur.com/build-plan` annotation, mounted with a downward API volume, which the admission policy now allows for that annotation only.
