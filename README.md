# kundy

Kundy 一体机的宿主机本地初始化服务。二进制名称为 `kundy`。

当前实现提供：

- 只使用 mDNS 的 `kundy.dhttp.net` DHTTP endpoint。
- `GET /` 本地初始化与状态页，页面风格与 Genmeta `whoami` 服务保持一致。
- `GET /healthz` 健康检查。
- `GET /api/status` 返回当前设备状态：`factory_ready`、`claiming` 或 `active`；已激活时额外返回用于本地信息页展示的 `display_name`。
- `GET /api/service/status` 通过本机 DHTTP endpoint 检测永久名字下的目标服务是否可访问。
- `GET /api/runtime/status` 返回宿主机 ConfigMap/Pishoo helper 的当前 generation 状态。
- `GET /api/certificate` 返回已激活设备证书的对象、签发者、有效期、序列号和 SHA-256 指纹。
- `POST /api/certificate/renew` 从本机私有状态读取原激活码，为同一个永久名字重新生成私钥、CSR 和证书。
- `GET /api/certificate/download` 下载已安装的公开证书链；该接口不会读取或返回私钥。
- `POST /api/activation/inspect` 检查激活码；请求只在内存中处理，并转发到配置的 CertServer。
- `POST /api/activation/names/check` 检查激活码对应子名字是否可用。
- `POST /api/activation/claim` 在本机生成私钥和 CSR，幂等调用 CertServer 完成永久名字绑定与证书签发，并按 gmutils 约定安装 DHTTP identity。首次安装身份时通过 `genmeta access` 为根路径写入匿名放行规则。
- 使用编译期固定的 UDP bind。默认 `*:3444` 动态匹配全部 IPv4/IPv6 网卡，使 mDNS 能随网卡变化重新绑定。
- `kundy activate` 提供与本地配置页相同的交互式激活流程，供无法跨网段访问 mDNS
  配置页时通过 SSH 使用。
- `kundy deactivate` 通过双重确认移除本机证书、DHTTP identity 和宿主机运行态；不会删除
  CertServer 上的激活码绑定，重新激活时仍使用原激活码。
- `sudo kundy setup --user <用户>` 为指定的既有非 root 用户配置并启动 Kundy 宿主机服务。

## 交互式 CLI 激活

使用与 daemon 相同的宿主机用户运行：

```bash
kundy activate
```

Kundy 不要求专用系统用户，也不需要 sudo。安装器为 Kundy 选择一个既有宿主机
运行账户，并确保 Pishoo 能发现该账户的 DHTTP Home；CLI 直接写入该账户的
`~/.kundy` 和 `~/.dhttp`。如果通过 SSH 激活，必须登录为同一账户，或先切换到该
账户；不能让 CLI 与 daemon 分别使用不同的 Home。

命令依次明文询问子名字和激活码，不要求预先提供命令行参数。两项输入完成后，它向 CertServer
验证激活码、读取所属域并检测子名字；检测通过后展示最终永久
名字，只有用户明确输入 `y` 才会申请证书、安装 DHTTP identity 并提交宿主机
runtime reconcile。激活码已经绑定名字时，CLI 会展示并继续恢复同一个名字，
不会允许使用其他子名字。

量产环境必须预装 gmutils。Kundy 只在目标 identity 尚无 `db/access.db` 时执行
`genmeta access --identity <永久名字> "/" allow "*?"`，使首次激活后的 SciFlow
入口默认允许匿名访问；已存在的访问数据库不会被覆盖，证书更新也不会重置管理员
后续调整过的规则。旧版本激活但尚未生成访问数据库的设备，会在下一次 runtime
reconcile 时补齐同一默认规则，并递增持久化的 runtime generation，确保 Pishoo
实际 reload 后才报告新一代运行态就绪。

CLI 与网页共用 `ActivationService`、幂等恢复状态和 `~/.kundy/operation.lock`，
因此即使 daemon 正在运行，两种入口也不会并发写激活状态。激活码是明文交互输入，
但不会进入命令行参数、shell history 或日志。无参数的 `kundy` 和显式
`kundy serve` 都继续启动本地配置服务。

`config/kundy.toml` 是构建配置，不是部署到一体机后读取的运行时配置。程序通过 `include_str!`
将它内嵌到二进制；修改 endpoint、CertServer 或页面展示配置后必须重新编译。
安装后的用户不需要维护 TOML，也没有 `KUNDY_CONFIG_PATH` 运行时覆盖入口。

当前发布构建使用正式 CertServer HTTP API 地址 `https://api.genmeta.net`，激活域为
`kundytest`；终端用户不能修改这些值。若需要本地开发，必须显式切换构建配置到
`http://127.0.0.1:3000` 和本地激活域，不要混用正式激活码。
激活码不属于构建配置，必须由用户在本地激活页输入，成功后才作为本机恢复凭据持久化。

共享的 `kundy.dhttp.net` 出厂证书位于 `tls/kundy.dhttp.net.crt.pem`，私钥固定从
Git 忽略的 `tls/kundy.dhttp.net.key.pem` 读取；两者通过 `include_bytes!` 一并内嵌。开发者本地
构建前需将私钥放到该路径，发布 workflow 则将 GitHub Actions Secret 写入同一路径，因此两条路径
使用相同的构建输入。它们只用于局域网内的 `kundy~` 出厂入口，不作为单台设备的授权凭据。该本地
入口不请求浏览器客户端证书；激活操作由随设备提供的激活码授权。Kundy 会检查 CertServer 返回的
设备证书是否与本机私钥、永久名字和 DHTTP 元数据一致。页面模板同样内嵌，因此安装产物运行时只依赖
`kundy` 二进制和可写的持久目录。

激活状态固定写入运行用户的 `~/.kundy`，程序首次需要持久化时以 `0700` 权限自行创建；
不再提供单独的状态目录配置。正式身份遵循 DHTTP 的用户 Home 约定，默认写入运行用户的
`~/.dhttp`，并兼容标准的 `DHTTP_HOME` 环境变量覆盖。签发前只保存恢复所需的永久私钥、CSR、
目标名字、claim/recover 操作类型、幂等键和激活码 SHA-256 摘要；签发成功后在权限为 `0600` 的
`~/.kundy/active.json` 中额外保存规范化的原激活码，用于手动更新证书以及证书过期、丢失或身份
损坏时恢复同一个永久名字。激活码不会通过页面、JSON API 或日志暴露。正式身份写入
`.dhttp/<子名字.所属域>/ssl`，并同步更新 `.dhttp/settings.toml` 的默认身份。中途断电或网络失败后，
使用相同名字和激活码重试即可继续同一次操作。

本地 `cargo run` 与 release 构建遵循同一规则。量产路径取决于安装时选择的既有运行
账户，不固定为 `/var/lib/kundy`，软件包也不创建专用用户或 Home。程序会在该账户
Home 中自行创建 `.kundy`；安装器只需保证 Kundy 与 Pishoo 对同一个 DHTTP Home 达成一致。

出厂 endpoint 显式使用 mDNS 并关闭 STUN，不访问 H3DNS 或 DHTTP bootstrap。因此 kundy 的
M1 构建不需要 `DHTTP_BOOTSTRAP_URL`、`DHTTP_CERT_SERVER_URL`、`DHTTP_H3_DNS_SERVER` 或
`DHTTP_MDNS_SERVICE`；mDNS service 使用依赖内置的 `_dhttp.local` 默认值。

本地激活页先填写子名字，再填写五组激活码。两项填写完整后，主按钮启用为“检测”；检测激活码
和子名字均通过后，同一按钮切换为“激活”。任何输入发生变化都会使检测结果失效。页面展示配置
来自 `config/kundy.toml`：

- `[ui].product_name`：页面展示的产品名称，当前为 `Kundy`。
- `[ui].activation_domain`：检测前展示的域，填写不带点号和 `~` 的根名字，当前为 `kundytest`。
- `[ui].service_path`：永久名字指向和运行状态检测使用的服务路径，当前为 `/sciflow-console/`。

`[certserver].base_url` 用于激活码检查、首次激活和同名证书重签。本地开发和生产构建分别把它
指向对应环境的 CertServer HTTP listener。

激活码检测成功后，页面始终改用 CertServer 返回的
`parent_domain`；最终可申请的域仍由 CertServer 根据激活码约束，页面默认值不参与授权。

激活完成后，`kundy~` 保持本地可访问并切换为本机信息页，不再显示或保留禁用的激活表单。
信息页中的永久名字可以直接打开配置的服务路径，并展示激活状态、服务运行状态和证书信息，不展示
`.dhttp.net` 内部后缀。页面加载后立即请求后端探测服务状态，随后按请求发起时间每 5 秒再次请求；
若上次探测超过 5 秒，则完成后立即继续，不并发堆积请求。后端使用本机 DHTTP endpoint 发起最长
8 秒的请求，收到 2xx 或 3xx 响应即视为运行正常。该检查覆盖名字解析、目标证书握手和服务响应，
但不代表用户已经完成 OAuth 登录。服务尚未可达或单次探测失败时，页面用中性的旋转状态和
“正在探测服务，请稍候”说明仍在持续探测，不显示红色故障信息；探测成功后切换为“运行正常”。
信息页加载和自动刷新只调用 GET 状态接口，不提交、恢复或改变宿主机运行态；只有用户明确点击
更新证书或取消激活时，才调用对应的写接口。

信息页底部的“取消激活”需要先确认弹窗，再输入原激活码；激活码只在本机与受保护状态比对，
不会重新提交到 CertServer。取消激活后页面回到首次激活状态；宿主机 helper 已安装但尚未完成
同步时会持久化本次停用意图并保留证书身份，CLI 重试、服务端控制器和进程重启都继续使用同一
generation，不会重新激活。禁用 ConfigMap 后，Kundy 会先移除该身份的 `server.conf`，
确保 Pishoo reload 不再加载它，再完成其余本地清理。helper 未安装的纯本地环境直接清理本地状态。

激活状态和正式设备身份不会内嵌：它们是每台设备独有的运行时数据，分别持久化到运行用户的
`~/.kundy` 和 DHTTP Home。

激活成功后，`kundy` 还会通过 `/run/kundy` 的固定文件协议请求宿主机 helper
更新 k3s 运行态并 reload Pishoo；它不会访问 kubeconfig 或直接修改 k3s。helper 的请求、
响应 schema、权限和幂等要求见 `HOST_RUNTIME_PROTOCOL.md`。helper 尚未安装时，证书身份仍
会正常保存，运行态结果显示为 `pending`。Kundy 的内部运行态控制器从持久化的期望状态继续操作；
该写流程不暴露为网页 API。

## 宿主机安装

`packaging/host` 提供量产集成包需要的 systemd units 和幂等安装脚本。它使用同一
个 `/usr/bin/kundy`：长期运行的 `kundy serve` 由选定的普通宿主机账户执行；两个
`kundy host ...` 内部子命令只能由 root oneshot 调用。网页和普通 CLI 不能传入
Kubernetes namespace、对象名、路径或命令。

从源码目录安装时，先构建或取得目标机器架构对应的 release binary，再在宿主机执行：

```bash
sudo ./packaging/host/install-kundy-host.sh --binary /path/to/kundy
```

默认运行账户是执行 `sudo` 前的 `SUDO_USER`，也可以通过 `--user` 选择另一个已经
存在的非 root 账户。安装器不创建专用用户或 Home；它要求 Pishoo、gmutils 和 k3s 已安装，把
运行账户加入现有 `dhttp` 组，安装并启动 `kundy.service`、两个固定 path/oneshot，
然后 reload Pishoo。重复执行同一命令可 repair unit 和二进制，不删除 `~/.kundy`
或 `~/.dhttp`。

runtime helper 会先确保逐机 OAuth Cookie Secret 存在，再创建或更新不属于 Flux
inventory 的 `flux-system/kundy-device-identity` ConfigMap。它拒绝旧 generation
回退和永久名字替换，并从 `cluster-settings/HOST` 返回当前一体机的内部入口。
Kundy 以普通运行用户权限生成正式 identity 的 `server.conf`，固定反代
`127.0.0.1:80` 并覆盖公开 Origin 相关请求头。Pishoo helper 会再次读取实际
ConfigMap，等待 `authentik` 与 `kundy-dhttp` 两个 Flux Kustomization 的
`lastHandledReconcileAt` 处理完当前 Kundy generation 且进入 Ready，再使用现有
`pishoo.service` 的 reload 接口；该判定不依赖可能被上层 GitOps 清除的临时
`requestedAt` annotation。

## Debian 打包

使用 `scripts/build-deb.sh` 在 Debian 容器构建环境中生成包。脚本默认优先使用
Podman，也支持 Docker；默认目标是
`x86_64-unknown-linux-gnu`，产物写入 `target/deb/<target>/`：

本地构建前需将出厂私钥放到 Git 忽略的 `tls/kundy.dhttp.net.key.pem`。脚本和容器都会直接读取
这个固定路径，不需要额外的环境变量。

```bash
./scripts/build-deb.sh
```

也可以显式构建其他已支持的 GNU/Linux 架构：

```bash
./scripts/build-deb.sh --target aarch64-unknown-linux-gnu
```

构建需要 Podman 或 Docker。两者都安装时默认使用 Podman，也可以显式选择：

```bash
CONTAINER_ENGINE=podman ./scripts/build-deb.sh
CONTAINER_ENGINE=docker ./scripts/build-deb.sh
```

`kundy` deb 安装 `/usr/bin/kundy`、内部 setup helper、主服务模板和四个固定的
path/oneshot units。apt 安装阶段不会猜测运行用户，也不会启用或启动这些 units；安装包后必须
显式选择既有非 root 账户：

```bash
sudo kundy setup --user <existing-user>
```

该命令把运行账户加入 `dhttp` 组，根据实际 Home 生成 `/etc/systemd/system/kundy.service`，
然后启用并启动 `kundy.service`、`kundy-runtime-config-apply.path` 和
`kundy-pishoo-reload.path`。重复执行可修复二进制、unit 配置和服务状态，不会触碰
`~/.kundy`、`~/.dhttp`、证书或私钥。

默认使用一个 Cargo 并行任务以降低低内存构建机被 OOM killer 终止的概率；内存充足时可以提高并行度：

```bash
KUNDY_BUILD_JOBS=2 ./scripts/build-deb.sh
```

在目标 Debian 主机上可以直接安装生成的包并执行 setup：

```bash
sudo apt install ./target/deb/x86_64-unknown-linux-gnu/kundy_*.deb
sudo kundy setup --user <existing-user>
```

## Debian 仓库发布

Kundy 的 GitHub Actions 发布流程参考 Pishoo，当前只发布 Linux `amd64`。本地可以先生成
带有版本清单的发布产物：

```bash
cargo xtask package --overwrite-manifest \
  deb --target x86_64-unknown-linux-gnu
```

产物位于 `target/x86_64-unknown-linux-gnu/release/deb/`，清单位于
`target/common/deb/manifest.toml`。该 xtask 打包命令需要运行环境提供
`dpkg-deb`（GitHub 的 Ubuntu runner 已包含）；macOS 本地构建请使用上面的
`scripts/build-deb.sh`。发布阶段使用：

```bash
cargo xtask publish s3 \
  --publish-report target/publish-reports/deb.toml \
  deb
```

非 `v*` tag 的 CI 运行使用 `--dry-run`，`v*` tag 才会真正更新
`download` bucket 下的 `ppa/genmeta` APT 仓库，并把 deb 上传到 GitHub Release。没有预发布后缀
的版本进入 `stable`，例如 `0.1.0-1`；带有 `-beta`、`-rc` 等预发布后缀的版本进入
`preview`，并转换为 Debian 的 `~` 版本形式。

发布 workflow 需要配置以下 GitHub Actions 变量和 secrets：

```text
变量（Settings → Secrets and variables → Actions → Variables）
XTASK_RELEASE_S3_ENDPOINT_URL

Secrets（Settings → Secrets and variables → Actions → Secrets）
XTASK_RELEASE_S3_ACCESS_KEY_ID
XTASK_RELEASE_S3_SECRET_ACCESS_KEY
XTASK_RELEASE_APT_SIGNING_KEY
XTASK_RELEASE_APT_SIGNING_PASSPHRASE
XTASK_RELEASE_KUNDY_BOOTSTRAP_PRIVATE_KEY
```

`XTASK_RELEASE_APT_SIGNING_KEY` 必须与已经发布到
`https://download.dhttp.net/ppa/key/public.key` 的公钥匹配。workflow 会从私钥计算 fingerprint
并在签名发布前校验。`XTASK_RELEASE_KUNDY_BOOTSTRAP_PRIVATE_KEY` 的值是完整的 PEM 私钥；workflow
仅在 `main` 推送和 tag 发版时把它写入被 Git 忽略的 `tls/kundy.dhttp.net.key.pem`，PR 则生成一次性
测试密钥用于编译验证。当前发布执行器使用 GitHub runner 自带的 Docker；本地
`scripts/build-deb.sh` 仍然支持 Podman 和 Docker。
