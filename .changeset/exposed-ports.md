---
default: patch
---

# A Build records the ports its image exposes

After a successful build, Builder reads the pushed image's config back from the registry and records its `ExposedPorts` in `status.exposedPorts` of the BuildRun and the Build, such as `["3000/tcp"]`. Zeabur suggests these ports on the service page. An image that declares none, such as a Railpack build, records none, and a failed read leaves the list empty without failing the build.
