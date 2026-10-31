// One stage of a release, as the shared release workflow of zeabur-oss/.github runs it, in the dev
// shell of the commit being released: `bun scripts/release.ts <stage> <version> ...`.
// @zeabur-oss/release (zeabur-oss/.github, release/) pushes, signs and verifies; the workflow
// itself checks the CHANGELOG.md section, signs SHA256SUMS and writes the notes. This script keeps
// what is Zeabur Builder's own.
//
//   check   <version>                           the version is the crate's and the chart's
//   build   <version> <os> <arch> <dist>        build the three images into <dist>, smoke them on k3s
//   publish <version> <artifacts> <published>   push and sign the images and the chart by digest
//   assets  <version> <artifacts> <assets>      copy the chart and digests.txt into <assets>
//   verify  <version> <os> <arch> <downloaded>  check the draft's images and chart
//
// <artifacts> holds the workflow's earlier artifacts: candidate-linux-<arch>/ from build and
// published/ from publish. Linux only; build is native (amd64 on x86_64, arm64 on aarch64).
import {
  Failure,
  absent,
  pushChart,
  pushImage,
  verifyChart,
  verifyImage,
} from "@zeabur-oss/release";
import { $ } from "bun";
import { rmSync } from "node:fs";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const scripts = join(root, "scripts");

// Each image and the chart value that pins it.
const images = [
  { name: "builder", key: "image" },
  { name: "builder-job", key: "jobImage" },
  { name: "builder-railpack", key: "railpackImage" },
];

function fail(message: string): never {
  console.error(message);
  process.exit(1);
}

// A directory of this run's own, removed when it ends, with an empty registry login. Helm reads
// the same file as crane.
async function scratch(): Promise<{
  dir: string;
  env: Record<string, string>;
}> {
  const dir = await mkdtemp(join(tmpdir(), "release-"));
  process.on("exit", () => rmSync(dir, { recursive: true, force: true }));
  const docker = join(dir, "docker");
  await mkdir(docker);
  return {
    dir,
    env: {
      DOCKER_CONFIG: docker,
      HELM_REGISTRY_CONFIG: join(docker, "config.json"),
    },
  };
}

async function check(version: string) {
  const plain = version.slice(1);
  const metadata =
    await $`cargo metadata --manifest-path ${join(root, "Cargo.toml")} --locked --no-deps --format-version 1`.json();
  const crate = metadata.packages.find(
    (p: { name: string }) => p.name === "zeabur-builder",
  ).version;
  const chart = Bun.YAML.parse(
    await Bun.file(join(root, "deploy/chart/Chart.yaml")).text(),
  ) as Record<string, unknown>;
  const problems = [];
  if (version !== `v${crate}`)
    problems.push(
      `the tag ${version} is not v${crate}, the crate zeabur-builder's version`,
    );
  if (String(chart.version) !== plain || String(chart.appVersion) !== plain)
    problems.push(
      `deploy/chart/Chart.yaml has version ${chart.version} and appVersion ${chart.appVersion}, not ${plain}`,
    );
  if (problems.length) fail(problems.join("\n"));
}

async function build(os: string, arch: string, dist: string) {
  if (os !== "linux") fail(`the images are Linux only, not ${os}`);
  const machine = (await $`uname -m`.text()).trim();
  if (
    process.platform !== "linux" ||
    { amd64: "x86_64", arm64: "aarch64" }[arch] !== machine
  )
    fail(`cannot build ${arch} here: each build is native and on Linux`);
  await mkdir(dist, { recursive: true });
  // mkShell sets SOURCE_DATE_EPOCH to 1980; the commit's time replaces it.
  process.env.SOURCE_DATE_EPOCH = (
    await $`git -C ${root} show -s --format=%ct HEAD`.text()
  ).trim();
  await $`${join(scripts, "build-images.sh")} zeabur-builder release-${arch}`;
  for (const { name } of images)
    await $`docker image save --output ${join(dist, `${name}.tar`)} zeabur-builder/${name}:release-${arch}`;
  // The images as built, with the chart of this commit, on k3s.
  await $`${join(scripts, "ci.sh")} kubernetes`.env({
    ...process.env,
    ZEABUR_CI_SHELL: "1",
    ZEABUR_CI_IMAGE_TAG: `release-${arch}`,
  });
}

async function publish(version: string, artifacts: string, out: string) {
  const plain = version.slice(1);
  const repository =
    process.env.GITHUB_REPOSITORY ?? fail("GITHUB_REPOSITORY is not set");
  const actor = process.env.GITHUB_ACTOR ?? fail("GITHUB_ACTOR is not set");
  const token = process.env.GH_TOKEN ?? fail("GH_TOKEN is not set");
  const owner = repository.split("/")[0].toLowerCase();
  const charts = `ghcr.io/${owner}/charts`;
  const { dir, env } = await scratch();
  $.env({ ...process.env, ...env });
  await mkdir(out, { recursive: true });

  if (
    (await $`gh release view ${version} --repo ${repository}`.nothrow().quiet())
      .exitCode === 0
  )
    fail(`release ${version} already exists; inspect it before trying again`);
  await $`crane auth login ghcr.io --username ${actor} --password-stdin < ${new Response(token)}`;
  // Every tag is refused before the first push, so a release never stops halfway on a tag that was
  // there from the start.
  const tags = [
    ...images.map(({ name }) => `ghcr.io/${owner}/${name}:${plain}`),
    `${charts}/zeabur-builder:${plain}`,
  ];
  await absent(...tags);

  const chart = join(dir, "chart");
  await cp(join(root, "deploy/chart"), chart, { recursive: true });
  await rm(join(out, "digests.txt"), { force: true });
  await rm(join(out, "subjects.json"), { force: true });
  const valuesFile = join(chart, "values.yaml");
  let values = await Bun.file(valuesFile).text();
  for (const { name, key } of images) {
    const { digest: index } = await pushImage(
      out,
      name,
      `ghcr.io/${owner}/${name}:${plain}`,
      {
        "linux/amd64": join(artifacts, "candidate-linux-amd64", `${name}.tar`),
        "linux/arm64": join(artifacts, "candidate-linux-arm64", `${name}.tar`),
      },
    );
    // A text edit of the key's `digest: ""`, so the comments and blank lines of values.yaml stay.
    values = values.replace(
      new RegExp(`^(${key}:\\n(?:  .*\\n|\\s*\\n)*?  digest: )""$`, "m"),
      `$1"${index}"`,
    );
    if (
      (Bun.YAML.parse(values) as Record<string, { digest: string }>)[key]
        .digest !== index
    )
      fail(`could not pin ${key}.digest in values.yaml`);
  }
  await Bun.write(valuesFile, values);

  // The chart at the version of Chart.yaml (checked equal to the release's by `check`).
  await $`helm lint --strict ${chart} --set registry.host=registry.example:5000`;
  await $`helm package ${chart} --destination ${out}`;
  await pushChart(
    out,
    `oci://${charts}`,
    join(out, `zeabur-builder-${plain}.tgz`),
  );
  await $`cat ${join(out, "subjects.json")} ${join(out, "digests.txt")}`;
}

async function assets(version: string, artifacts: string, dir: string) {
  for (const file of [`zeabur-builder-${version.slice(1)}.tgz`, "digests.txt"])
    await cp(join(artifacts, "published", file), join(dir, file));
}

async function verify(
  version: string,
  os: string,
  arch: string,
  downloaded: string,
) {
  if (os !== "linux") fail(`the release is Linux only, not ${os}`);
  console.log(`verifying ${version} on ${os}/${arch}`);
  // A fresh, empty registry configuration: a publisher's login must not hide a private package.
  $.env({ ...process.env, ...(await scratch()).env });
  const chart = join(downloaded, `zeabur-builder-${version.slice(1)}.tgz`);
  const digests = join(downloaded, "digests.txt");
  const values = Bun.YAML.parse(
    await $`helm show values ${chart}`.text(),
  ) as Record<string, { repository: string; digest: string }>;
  for (const { name, key } of images) {
    // The chart installs this index by default.
    const index = String(
      await verifyImage(digests, name, ["linux/amd64", "linux/arm64"]),
    );
    const pinned = `${values[key].repository}@${values[key].digest}`;
    if (pinned !== index) fail(`the chart's ${key} is ${pinned}, not ${index}`);
  }
  await verifyChart(digests, chart);
  await $`helm template builder ${chart} --set registry.host=registry.example:5000 --set registry.tokenKey.namespace=zot --set registry.tokenKey.secret=zot-token-signer > /dev/null`;
  console.log(`release ${version} verified`);
}

const [stage, version, ...rest] = process.argv.slice(2);
const usage =
  "usage: bun scripts/release.ts check|build|publish|assets|verify <version> ... (see the script's header)";
if (!version?.startsWith("v")) fail(usage);
const args = rest.map((a, i) =>
  (stage === "build" || stage === "verify") && i < 2 ? a : resolve(a),
);
try {
  if (stage === "check" && args.length === 0) await check(version);
  else if (stage === "build" && args.length === 3)
    await build(args[0], args[1], args[2]);
  else if (stage === "publish" && args.length === 2)
    await publish(version, args[0], args[1]);
  else if (stage === "assets" && args.length === 2)
    await assets(version, args[0], args[1]);
  else if (stage === "verify" && args.length === 3)
    await verify(version, args[0], args[1], args[2]);
  else fail(usage);
} catch (e) {
  if (e instanceof Failure) fail(e.message);
  // The command has printed its own error.
  if (e instanceof $.ShellError) process.exit(e.exitCode);
  throw e;
}
