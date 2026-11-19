# zeabur-builder-agent

Writes a Dockerfile for a repository and trial-builds it on a buildkitd. It is the planner of
the `agent` builder ([#152](https://github.com/zeabur-oss/builder/issues/152)).

```text
ANTHROPIC_API_KEY=… zeabur-builder-agent --context <dir> --buildkit <addr> [--tlsdir <dir>] \
  --provider anthropic --model claude-opus-5-5 [--base-url <url>] [--previous <dir>] [--out <dir>]
```

It takes the first of:

1. The repository's `Dockerfile`, as is.
2. The last plan (`--previous`), when it still builds.
3. A Dockerfile the agent writes. The agent reads the repository with `list`, `read` and
   `grep`, builds with `build`, pages through a failed build's log with `read_build_log`, and
   ends with `finish` after a build succeeded or `stop` with a reason.

It prints the Dockerfile, writes it to `<out>/Dockerfile` with `<out>/Dockerfile.dockerignore`,
and ends with the info object on stdout and in `<out>/info.json`. It exits 0 with a plan, 2 when
it stopped with a reason, and 1 on an error of its own.

`--provider` is a provider of Rig's registry: `anthropic` uses the Messages API, `openai` the
Responses API, `gcp.gemini` the Gemini API, and a gateway such as `openrouter` Chat Completions.
`--model` is the provider's model name, listed in Rig's catalogue or not. Rig reads the key from
the provider's own variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY` …) and the base URL from
`--base-url`, else the provider's variable where it has one (`ANTHROPIC_BASE_URL`,
`OPENAI_BASE_URL`). Requests go through hyper and rustls with the system's CA bundle.

## Limits

The host keeps them, not the prompt: 40 model calls, 4 failed builds, 10 minutes per build, 20
minutes per run, 1,000,000 input tokens and 200 KiB of conversation. Tool results are at most 12
KiB, build log pages 8 KiB.

## Trial builds

`buildctl build` with no output, the bundled Dockerfile frontend pinned
(`--opt cmdline=dockerfile.v0`, so `# syntax` loads nothing), no entitlements, and a source
policy that denies everything but the two local sources and images of the allowed registries
(`--registry`, default `docker.io`, `ghcr.io`, `quay.io`, `gcr.io`). Every build ignores `.git`,
`.env` files and other credentials.
