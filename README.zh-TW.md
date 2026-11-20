<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/cover-zh-TW-dark.avif">
  <img alt="Zeabur Builder：程式碼推上 GitHub，自動打包成可以上線的版本。Dockerfile 或 Python、PHP、Ruby、Node.js、Go 程式碼變成映像 main-1761058800-3e1a9c2，然後執行中。" src="docs/images/cover-zh-TW-light.avif">
</picture>

<!-- Language names are written in their own script. -->
<!-- vale ZeaburZhTW.Script = NO -->
[English](README.md) · **繁體中文** · [简体中文](README.zh-CN.md)
<!-- vale ZeaburZhTW.Script = YES -->

# Zeabur Builder

**程式碼推上 GitHub，Zeabur 開源版就會部署，跟在 Zeabur 上一樣**。Zeabur Builder 是 Zeabur 開源版裡負責把 Git 儲存庫變成可執行應用程式的部分：它會判斷程式碼用的語言、建置出映像檔，之後分支一有更新就會重新建置。

## 使用 Zeabur 開源版？

不需要另外安裝這個儲存庫的任何東西。

- **zeabur-installer** 預設就會安裝 Zeabur Builder。個人安裝時，在「選擇元件」勾選 **Zeabur Builder**。
- **已經在用 Zeabur 開源版，但還沒有裝**？打開**叢集** → **元件**，找到 Zeabur Builder 後按**安裝**，畫面會列出要在伺服器上執行的指令。

要部署時，在專案裡選**新增** → **從 Git 部署**，見[從 Git 建置並部署服務](https://oss.zeabur.com/zh-TW/docs/zeabur/deploy/git/)。安裝 Zeabur 開源版和管理服務的方式，見 [Zeabur 開源版文件](https://oss.zeabur.com/zh-TW/docs/zeabur/)。

## 它能幫你做什麼

- **自動判斷語言**。用 [Railpack](https://railpack.com) 建置 Node.js、Python、Go、PHP、Ruby、Java、Rust、Elixir、Deno、.NET、Gleam、C++、Zig 應用程式、靜態網站和 shell 腳本，不需要 Dockerfile。儲存庫裡有 Dockerfile 時，就照 Dockerfile 建置。
- **分支更新就重新建置**。推送到分支後就會建置新版本。搭配 Zeabur 開源版和 Keel，服務接著會換成新版本。
- **建置用的機密變數不會留在映像檔裡**。建置變數（例如下載私有套件用的權杖）以機密方式傳給建置，除非建置過程自己把值複製進去，否則不會存進映像檔。
- **在你的伺服器上執行**。建置和快取都留在你自己的機器和 registry 上。

## 單獨使用

Zeabur Builder 也可以不搭配 Zeabur 開源版使用。它是 Kubernetes operator，用 BuildKit 從 Git 建置容器映像檔，再把 digest 交給你的部署工具。

`Build` 指定一個 Git 儲存庫和一個建置器：

```yaml
apiVersion: builder.zeabur.com/v1alpha1
kind: Build
metadata: {name: whoami, namespace: team-a}
spec:
  source: {type: git, git: {url: https://github.com/traefik/whoami}}
  builderRef: dockerfile
```

ref 上每出現一個新 commit，控制器就會建置映像檔，推送到 `<registry.host>/<namespace>/<name>`，再以 `<repository>:<tag>@<digest>` 的格式寫入 `status.latestImage`。不寫 `ref` 時會追蹤儲存庫的預設分支，`status.resolvedRef` 會顯示是哪個分支。

- **兩種建置器**。`dockerfile` 依照你的 Dockerfile 建置。`railpack` 用 [Railpack](https://railpack.com) 為沒有 Dockerfile 的儲存庫規劃建置；建出的映像檔以 uid 1000 執行，可以在 Pod Security `restricted` 下啟動。
- **部署工具能排序的 tag**。`main-1761058800-3e1a9c2`（分支、Unix 時間、commit），每次建置一個，越新的排越後面。可以用 `spec.output.tagTemplate` 和 `extraTags` 修改。
- **每個 Namespace 一個 buildkitd**。每個要建置的 Namespace 都有自己的 buildkitd、快取、憑證機構和 registry 路徑。快取在兩次建置之間留在 buildkitd 的磁碟區上，所以 `RUN --mount=type=cache` 的內容會保留下來，而且不會推送到 registry。buildkitd 閒置 10 分鐘後會縮減到零。
- **建置變數以機密方式傳遞**。`spec.env` 以 BuildKit secret 傳給建置，值可以直接填寫，也可以讀取 Secret 的鍵。
- **上傳的資料夾**。用戶端可以把資料夾以 OCI artifact 推送到 registry，`spec.source.archive` 再依 digest 建置這個資料夾（見[從上傳的資料夾建置](docs/zh-TW/builds/archive-source.mdx)）。
- **輕量**。一個控制器、每個要建置的 Namespace 一個 buildkitd、每次建置一個 Job、三種自訂資源。Job 執行 `buildctl`，不需要特殊權限。

buildkitd 在 user namespace 裡以 privileged 模式執行。同一個 Namespace 裡的建置彼此信任。要建置你不信任的人寫的程式碼，光靠 Namespace 隔離不夠，要用 `--buildkitd-runtime-class` 讓 buildkitd 在沙盒執行環境裡執行，並放在[專用的節點](docs/zh-TW/builds/build-nodes.mdx)上。信任模型見[安全性](docs/zh-TW/reference/security.mdx)。

Zeabur Builder 只負責建置映像檔。測試、部署和日誌儲存交給你的 pipeline、部署工具和日誌收集工具。

## 文件

Zeabur 開源版使用者：

- [從 Git 建置並部署服務](https://oss.zeabur.com/zh-TW/docs/zeabur/deploy/git/)
- [檢視與安裝叢集元件](https://oss.zeabur.com/zh-TW/docs/zeabur/cluster/components/)

單獨使用 Zeabur Builder：

- [安裝](docs/zh-TW/get-started/install.mdx)和[建立第一個 Build](docs/zh-TW/get-started/first-build.mdx)
- [觸發建置](docs/zh-TW/builds/triggers.mdx)、[私有儲存庫](docs/zh-TW/builds/private-repos.mdx)、[從上傳的資料夾建置](docs/zh-TW/builds/archive-source.mdx)、[部署](docs/zh-TW/builds/rollout.mdx)、[外部 registry](docs/zh-TW/builds/external-registries.mdx) 和[建置節點](docs/zh-TW/builds/build-nodes.mdx)
- [Registry 權杖](docs/zh-TW/operations/registry-tokens.mdx)、[控制器的權限](docs/zh-TW/operations/controller-permissions.mdx)和[發行檔](docs/zh-TW/operations/releases.mdx)
- [運作方式](docs/zh-TW/reference/how-it-works.mdx)和[安全性](docs/zh-TW/reference/security.mdx)

## 參與貢獻

見 [CONTRIBUTING.md](CONTRIBUTING.md)。回報安全性問題的方式見 [SECURITY.md](SECURITY.md)，裡面也說明了信任模型。

## 授權

[Apache-2.0](LICENSE)。
