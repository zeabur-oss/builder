<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/cover-zh-CN-dark.avif">
  <img alt="Zeabur Builder：代码推到 GitHub，自动打包成可以上线的版本。Dockerfile 或 Python、PHP、Ruby、Node.js、Go 代码变成镜像 main-1761058800-3e1a9c2，然后运行中。" src="docs/images/cover-zh-CN-light.avif">
</picture>

<!-- Language names are written in their own script. -->
<!-- vale ZeaburZhCN.Script = NO -->
[English](README.md) · [繁體中文](README.zh-TW.md) · **简体中文**
<!-- vale ZeaburZhCN.Script = YES -->

# Zeabur Builder

**代码推到 GitHub，Zeabur 开源版就会部署，和在 Zeabur 上一样**。Zeabur Builder 是 Zeabur 开源版中把 Git 仓库变成可运行应用的部分：它识别代码使用的语言，构建出镜像，之后分支一有更新就会重新构建。

## 在用 Zeabur 开源版？

不需要另外安装这个仓库里的任何东西。

- **zeabur-installer** 默认安装 Zeabur Builder。个人版安装时，在“选择组件”中勾选 **Zeabur Builder**。
- **已经在用 Zeabur 开源版，但还没装**？打开**集群** → **组件**，找到 Zeabur Builder 后点击**安装**，页面会列出需要在服务器上运行的命令。

部署时，在项目中选择**新增** → **从 Git 部署**，见[从 Git 构建并部署服务](https://oss.zeabur.com/zh-CN/docs/zeabur/deploy/git/)。安装 Zeabur 开源版和管理服务的方法，见 [Zeabur 开源版文档](https://oss.zeabur.com/zh-CN/docs/zeabur/)。

## 它能帮你做什么

- **自动识别语言**。用 [Railpack](https://railpack.com) 构建 Node.js、Python、Go、PHP、Ruby、Java、Rust、Elixir、Deno、.NET、Gleam、C++、Zig 应用、静态网站和 shell 脚本，不需要 Dockerfile。仓库里有 Dockerfile 时，按 Dockerfile 构建。
- **分支更新就重新构建**。推送到分支后就会构建新版本。配合 Zeabur 开源版和 Keel，服务随后会切换到新版本。
- **构建用的机密变量不会留在镜像里**。构建变量（例如下载私有包用的令牌）以机密方式传给构建，除非构建过程自己把值复制进去，否则不会存进镜像。
- **在你的服务器上运行**。构建和缓存都留在你自己的机器和镜像仓库上。

## 单独使用

Zeabur Builder 也可以脱离 Zeabur 开源版使用。它是 Kubernetes operator，用 BuildKit 从 Git 构建容器镜像，再把 digest 交给你的部署工具。

`Build` 指定一个 Git 仓库和一个构建器：

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: whoami, namespace: team-a}
spec:
  source: {git: {url: https://github.com/traefik/whoami}}
  builderRef: dockerfile
```

ref 上每出现一个新提交，控制器就构建镜像，推送到 `<registry.host>/<namespace>/<name>`，再以 `<repository>:<tag>@<digest>` 的格式写入 `status.latestImage`。不写 `ref` 时跟踪仓库的默认分支，`status.resolvedRef` 会显示是哪个分支。

- **两种构建器**。`dockerfile` 按你的 Dockerfile 构建。`railpack` 用 [Railpack](https://railpack.com) 为没有 Dockerfile 的仓库规划构建；构建出的镜像以 uid 1000 运行，可以在 Pod Security `restricted` 下启动。
- **部署工具能排序的标签**。`main-1761058800-3e1a9c2`（分支、Unix 时间、提交），每次构建一个，越新的排在越后面。可以用 `spec.output.tagTemplate` 和 `extraTags` 修改。
- **每个 Namespace 一个 buildkitd**。每个要构建的 Namespace 都有自己的 buildkitd、缓存、证书颁发机构和镜像仓库路径。缓存在两次构建之间保留在 buildkitd 的卷上，所以 `RUN --mount=type=cache` 的内容会保留下来，并且不会推送到镜像仓库。buildkitd 空闲 10 分钟后缩容到零。
- **构建变量以机密方式传递**。`spec.env` 以 BuildKit secret 传给构建，值可以直接填写，也可以读取 Secret 中的键。
- **上传的文件夹**。客户端可以把文件夹以 OCI artifact 推送到镜像仓库，`spec.source.archive` 再按 digest 构建这个文件夹（见[从上传的文件夹构建](docs/zh-CN/builds/archive-source.mdx)）。
- **轻量**。一个控制器、每个要构建的 Namespace 一个 buildkitd、每次构建一个 Job、三种自定义资源。Job 运行 `buildctl`，不需要特权。

buildkitd 在 user namespace 中以 privileged 模式运行。同一个 Namespace 中的构建互相信任。要构建你不信任的人写的代码，仅靠 Namespace 隔离不够，要用 `--buildkitd-runtime-class` 让 buildkitd 在沙箱运行时中运行，并放在[专用节点](docs/zh-CN/builds/build-nodes.mdx)上。信任模型见[安全性](docs/zh-CN/reference/security.mdx)。

Zeabur Builder 只构建镜像。测试、部署和日志存储交给你的 pipeline、部署工具和日志收集工具。

## 文档

Zeabur 开源版用户：

- [从 Git 构建并部署服务](https://oss.zeabur.com/zh-CN/docs/zeabur/deploy/git/)
- [查看和安装集群组件](https://oss.zeabur.com/zh-CN/docs/zeabur/cluster/components/)

单独使用 Zeabur Builder：

- [安装](docs/zh-CN/get-started/install.mdx)和[创建第一个 Build](docs/zh-CN/get-started/first-build.mdx)
- [触发构建](docs/zh-CN/builds/triggers.mdx)、[私有仓库](docs/zh-CN/builds/private-repos.mdx)、[从上传的文件夹构建](docs/zh-CN/builds/archive-source.mdx)、[部署](docs/zh-CN/builds/rollout.mdx)、[外部镜像仓库](docs/zh-CN/builds/external-registries.mdx)和[构建节点](docs/zh-CN/builds/build-nodes.mdx)
- [Registry 令牌](docs/zh-CN/operations/registry-tokens.mdx)、[控制器的权限](docs/zh-CN/operations/controller-permissions.mdx)和[发行文件](docs/zh-CN/operations/releases.mdx)
- [工作原理](docs/zh-CN/reference/how-it-works.mdx)和[安全性](docs/zh-CN/reference/security.mdx)

## 参与贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)。报告安全问题的方式见 [SECURITY.md](SECURITY.md)，其中也说明了信任模型。

## 许可证

[Apache-2.0](LICENSE)。
