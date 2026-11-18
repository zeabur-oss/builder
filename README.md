<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/cover-dark.avif">
  <img alt="Zeabur Builder: push your code to GitHub. It’s packaged, ready to go live. A Dockerfile or Python, PHP, Ruby, Node.js or Go code becomes the image main-1761058800-3e1a9c2, and it is running." src="docs/images/cover-light.avif">
</picture>

**English** · [繁體中文](README.zh-TW.md) · [简体中文](README.zh-CN.md)

# Zeabur Builder

**Push your code to GitHub and Zeabur deploys it, as on zeabur.com.** Zeabur Builder is the part of Zeabur
that turns a Git repository into an app that can run: it finds the language your code is in, builds
it, and builds again when the branch changes.

## Using Zeabur?

You don't need to install anything from this repository.

- **zeabur-installer** installs Zeabur Builder by default. On a personal installation, select
  **Zeabur Builder** in “Choose components.”
- **Already running Zeabur without it?** Open **Cluster** → **Components**, find Zeabur Builder and
  click **Install**. Zeabur shows the command to run on your server.

To deploy, open a project and select **Add** → **Deploy from Git**:
[Build and deploy a service from Git](https://oss.zeabur.com/docs/zeabur/deploy/git/). The rest of
Zeabur, from installing it on a server to running your services, is in the
[Zeabur documentation](https://oss.zeabur.com/docs/zeabur/).

## What it does for you

- **Finds your language.** With [Railpack](https://railpack.com), it builds Node.js, Python, Go,
  PHP, Ruby, Java, Rust, Elixir, Deno, .NET, Gleam, C++ and Zig apps, static sites and shell
  scripts without a Dockerfile. If your repository has a Dockerfile, it builds that instead.
- **Rebuilds when the branch changes.** Push to the branch and a new version is built. With Zeabur and Keel,
  the service then switches to it.
- **Keeps build secrets out of the image.** Build variables, such as a token for private packages,
  reach the build as secrets, so they aren't stored in the image unless your build copies them
  there.
- **Runs on your server.** Builds and their cache stay on your own machines and your own registry.

## Use it on its own

Zeabur Builder also works without Zeabur. It is a Kubernetes operator that builds container images
from Git with BuildKit and hands the digest to your rollout tool.

A `Build` names a Git repository and a builder:

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: whoami, namespace: team-a}
spec:
  source: {git: {url: https://github.com/traefik/whoami}}
  builderRef: dockerfile
```

For every new commit on the ref, the controller builds an image, pushes it to
`<registry.host>/<namespace>/<name>` and writes it to `status.latestImage` as
`<repository>:<tag>@<digest>`. Leave out `ref` to follow the repository's default branch;
`status.resolvedRef` says which branch that was.

- **Two builders.** `dockerfile` builds your Dockerfile. `railpack` plans the build with
  [Railpack](https://railpack.com) for repositories without one; its images run as uid 1000, so
  they start under Pod Security `restricted`.
- **Tags rollout tools can sort.** `main-1761058800-3e1a9c2` (branch, Unix time, commit), one per
  build, newest last. `spec.output.tagTemplate` and `extraTags` change them.
- **One buildkitd per namespace.** Each namespace that builds gets its own buildkitd, cache,
  certificate authority and registry path. The cache stays on buildkitd's volume between builds,
  so `RUN --mount=type=cache` keeps its contents; it never goes to the registry. An idle
  buildkitd scales to zero after 10 minutes.
- **Build variables as secrets.** `spec.env` reaches the build as BuildKit secrets, from a value
  or a Secret key.
- **Uploaded folders.** `spec.source.archive` builds a folder that a client pushed to the
  registry as an OCI artifact, by digest ([archive sources](docs/builds/archive-source.mdx)).
- **Small.** One controller, a buildkitd per building namespace, one Job per build, three custom
  resources. The Job runs `buildctl` and needs no privileges.

buildkitd runs privileged, inside a user namespace. Builds in one namespace trust each other, and
namespaces alone don't isolate code from people you don't trust: for that, run buildkitd in a
sandbox runtime with `--buildkitd-runtime-class` and on [nodes of its own](docs/builds/build-nodes.mdx).
[Security](docs/reference/security.mdx) has the trust model.

Zeabur Builder builds images and stops there. Tests, deployment and log storage belong to your
pipeline, rollout tool and log collector.

## Documentation

For Zeabur users:

- [Build and deploy a service from Git](https://oss.zeabur.com/docs/zeabur/deploy/git/)
- [View and install cluster components](https://oss.zeabur.com/docs/zeabur/cluster/components/)

Using Zeabur Builder on its own:

- [Install](docs/get-started/install.mdx) and [your first Build](docs/get-started/first-build.mdx)
- [Triggers](docs/builds/triggers.mdx), [private repositories](docs/builds/private-repos.mdx), [archive sources](docs/builds/archive-source.mdx), [rollout](docs/builds/rollout.mdx), [external registries](docs/builds/external-registries.mdx) and [build nodes](docs/builds/build-nodes.mdx)
- [Registry tokens](docs/operations/registry-tokens.mdx), [controller permissions](docs/operations/controller-permissions.mdx) and [releases](docs/operations/releases.mdx)
- [How it works](docs/reference/how-it-works.mdx) and [security](docs/reference/security.mdx)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues as described in
[SECURITY.md](SECURITY.md), which also describes the trust model.

## License

[Apache-2.0](LICENSE).
