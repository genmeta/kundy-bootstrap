# Kundy 简略开发计划

## 1. 项目与产物命名

- 源码仓库、Cargo package 和 deb 包统一使用 `kundy`，表达它是宿主机初始化服务。
- 最终可执行文件命名为 `kundy`，Cargo 中显式配置：

  ```toml
  [[bin]]
  name = "kundy"
  path = "src/main.rs"
  ```

- systemd 服务统一命名为 `kundy.service`，以后使用固定的 `ExecStart=/usr/bin/kundy` 启动。
- V1 只有一个长期运行进程；若后续需要 `check`、`version` 等命令，再在同一 `kundy` CLI 下增加子命令，不新增第二个 daemon binary。

## 2. DHTTP 与 h3x 依赖策略

- 第一阶段参考 `/Users/x/code/genmeta/whoami` 的独立服务结构，直接依赖发布到 crates.io 的 `dhttp`，不使用 `/Users/x/...` sibling path。
- `dhttp` 已公开 re-export `h3x`。只使用 DHTTP 服务端所需类型时，通过 `dhttp::h3x` 引用，不再单独声明 `h3x`，避免 Cargo 同时解析出两套不兼容的 h3x 类型。
- 若以后确实需要 `dhttp` 未开启的 h3x feature，再把直接 `h3x` 依赖锁到与所选 dhttp 完全兼容的版本；该决定必须通过 `cargo tree -d` 局部检查，不能凭版本号猜测。
- 当前采用正式版 `dhttp = 0.6.1`（带 `netwatcher` feature），锁文件同步使用 `h3x = 0.6.1`、`dyns = 0.7.1` 和 `dquic = 0.7.1`。正式发布前仍需与 appliance release 中 Pishoo/gmutils 使用的版本做一次兼容性 spike，再冻结版本并提交 `Cargo.lock`。
- 出厂 endpoint 只启用 mDNS，并在 `DhttpNetwork` 上显式关闭 STUN，避免访问 H3DNS 或 DHTTP bootstrap。M1 不配置 DHTTP 基础设施环境变量，mDNS service 使用依赖内置的 `_dhttp.local` 默认值；设备激活后的 H3DNS 与身份配置由后续运行态接管。

## 3. 第一里程碑：只发布 `kundy~` 的本地服务

目标是先得到可由 AnySee 在局域网打开、不会向 H3DNS 汇报的最小可执行程序。

- M1 通过 `include_bytes!` 内嵌共享 `kundy` bootstrap 证书、私钥和 DHTTP CA；它们只用于本地入口，不作为设备授权凭据。
- `config/kundy.toml` 是唯一构建配置，通过 `include_str!` 内嵌，不作为运行时配置安装到设备；页面模板也直接内嵌到二进制。
- 使用 Axum 构建同源页面和 JSON API，再通过 `dhttp::h3x::hyper::TowerService` 挂到 H3 endpoint。
- 使用独立、固定且不与 Pishoo 冲突的 UDP 端口；端口冲突时明确失败，不随机换端口。
- DNS plan 必须显式只加入 mDNS：

  ```rust
  let mut dns_plan = DhttpDnsPlan::new();
  dns_plan.push_dns(DnsScheme::Mdns);
  ```

- 不能使用空的 `DhttpDnsPlan::new()` 作为最终配置，因为空 plan 会回落到 `H3 + mDNS + System` 默认发布组合。
- 第一版页面采用 `whoami` 的 Genmeta 视觉约定，只向用户显示设备生命周期状态和服务健康状态，不暴露 mDNS、H3DNS 等网络实现细节。
- 用户点击“检测”后，页面通过同源 API 依次完成激活码检查与子名字可用性检查；这两个动作没有副作用，激活码不写磁盘、不写日志。
- CertServer URL 必须由构建配置显式指定；开发构建指向 `127.0.0.1:3000`，正式发布构建固定为线上地址，运行时不允许覆盖。

验收：

- AnySee 可以通过 `https://kundy~` 打开页面。
- H3DNS 中不存在 `kundy.dhttp.net` publication。
- 进程重启后仍使用相同端口和内嵌身份。
- Pishoo 同时运行时没有 UDP bind 冲突。

## 4. 第二里程碑：状态机与 CertServer 激活（当前已完成身份认领部分）

- 当前代码按 `config`、`dhttp_server`、`api`、`cli`、`activation` 和 `runtime`
  拆分；状态与 identity 写入仍由 `activation` 聚合，不提前增加空模块。
- 当前对外状态简化为 `factory_ready -> claiming -> active`。`active` 表示永久名字与正式 DHTTP identity 已配置完成；k3s 和 SciFlow 运行态就绪度后续使用独立状态表达，不混入证书认领状态。
- `kundy activate` 提供无参数的交互式 SSH 激活入口：依次读取明文子名字和激活码，
  再读取 CertServer 所属域，检测成功并由用户确认后复用同一
  `ActivationService::claim` 流程。量产机不创建固定的 `kundy-bootstrap` 用户；CLI
  和 daemon 使用安装器选择的同一个既有宿主机账户及 Home，Pishoo 必须能发现该
  账户的 DHTTP Home。
- 网页、CLI、证书更新和 runtime reconcile 除进程内 mutex 外，共用
  `~/.kundy/operation.lock` 文件锁，避免多个 `kundy` 进程同时修改 pending、
  active 和 DHTTP identity 状态。
- 首次签发前，产品密钥只存在于当前浏览器请求、pending 操作和 CertServer 请求内；签发成功后把规范化原激活码写入权限为 `0600` 的 active 状态，仅用于手动更新证书或恢复同一个永久名字，任何页面、API 和日志都不得返回该值。
- 设备本地生成正式私钥和 CSR；签发前保存幂等键、目标名字、私钥和 CSR，签发完成后保存 operation ID 与设备名字，以支持掉电恢复。
- CertServer 完成永久绑定后，即使后续失败也只能继续或恢复同一个 `xxxxx.kundy`，不能重新选名字。
- 同源页面不向日志写入激活码、CSR 或私钥，JSON API 统一限制为 16 KiB 请求体。显式 Origin 校验和节流仍需在正式镜像验收前补齐。

## 5. 第三里程碑：按 gmutils 约定安装正式身份（已完成基础写入）

- 正式 identity 位于实际运行用户的 `~/.dhttp/<name>`；不固定用户名或绝对 Home 路径。
- 激活恢复状态固定在运行用户的 `~/.kundy`；debug 与 release 使用相同约定，进程自行以 `0700` 创建目录，不再配置独立 state 路径。
- 使用 DHTTP Home 的 profile 和 `ssl` 目录约定，不设计 kundy 私有的 Pishoo identity 格式；runtime helper 从 Flux `cluster-settings/HOST` 返回内部入口后，kundy 以普通运行用户权限原子生成固定格式的 `server.conf`。
- 使用 DHTTP Home 的事务式 `save_identity` 写入证书和私钥，写入后通过签名验证证书与私钥匹配。
- 已有 profile 的恢复和证书更新遵守 gmutils/DHTTP Home `save_identity` 的 stage、backup、fsync 和 rollback 语义，并保留 `server.conf`。
- 直接复用公开 Rust API，不把 `genmeta identity apply` 当作运行时子进程。
- 量产镜像保证预装 gmutils。首次身份安装通过 `genmeta access --identity <永久名字> "/" allow "*?"` 初始化根路径匿名放行；仅在 `db/access.db` 不存在时执行，因此证书更新和运行态 reconcile 不覆盖管理员后续调整的规则。为旧版设备补建访问库时先递增并持久化 runtime generation，再请求 helper，确保 Pishoo 必须实际 reload 新规则。
- 手动更新始终生成新私钥和 CSR，并使用保存的原激活码调用 `/v2/activation/recover` 重签同一个永久名字；不依赖当前证书进行 mTLS 续签，也不要求用户重新输入激活码。
- 写入 `.dhttp/settings.toml` 默认 identity。加入 `dhttp` 组后，由现有 Pishoo worker discovery 发现该 identity；kundy 不修改 Pishoo 发布模式。

## 6. 第四里程碑：宿主机与 k3s 集成

- kundy 始终以非 root 用户运行，不持有 sudo 或 k3s kubeconfig。
- 激活时只原子写入包含设备名字和 generation 的严格 schema `runtime-config.request`。
- `kundy` deb 提供 root-owned runtime-config oneshot，只确保固定 Cookie Secret、更新固定的 `kundy-device-identity` ConfigMap、读取固定的 `cluster-settings/HOST`，并请求 `authentik` 与 `kundy-dhttp` 两个 Flux Kustomization reconcile。
- Flux 只读取 identity ConfigMap，不拥有或回写它；Pishoo helper 必须等到两个 Kustomization 已处理本次 `requestedAt` 且 `Ready=True`，不能把 ConfigMap 已写入误报为服务就绪。
- 正式 identity 就绪后写入 Pishoo reload marker，由另一个固定 root oneshot 执行全量 reload；成功标准是 Pishoo 实际加载目标名字和证书。
- `/run` 请求在重启后可以从运行用户 Home 中的持久状态重新生成，不能因临时 marker 丢失而卡死激活。

当前实现已冻结并接入第一版文件协议，详见 `HOST_RUNTIME_PROTOCOL.md`：

- `kundy` 在证书/identity 安装和手动更新后提交 `runtime-config.request`，
  再提交 `pishoo-reload.request`；请求只包含 `device_name` 和 operation
  generation，不包含激活码或私钥。
- `/api/runtime/status` 和 `/api/runtime/reconcile` 暴露的是运行态结果，不
  改变证书激活状态；helper 不存在时结果为 `pending`，因此开发机仍可以只
  验证本地 mDNS 页面。
- root helper 已作为同一 `kundy` 二进制的固定内部子命令实现，
  `packaging/host` 提供随 `kundy` deb 安装的内部 setup 程序与 systemd units。
  helper 只接受固定文件协议并执行固定 ConfigMap/Cookie Secret/Flux reconcile/Pishoo 操作；Flux
  正式模板继续由量产 Flux 仓库管理。

## 7. 软件包与 systemd

- 独立 `kundy` deb 包已落在 `packaging/deb`，拥有 `/usr/bin/kundy`、内部 setup helper、systemd 模板和固定 units，不创建专用用户或 Home；Debian 容器构建脚本支持 GNU/Linux 目标和 Cargo lockfile，可使用 Podman 或 Docker。`sudo kundy setup --user <用户>` 显式选择既有运行账户，配置 Kundy、runtime 目录和 systemd 生命周期，并确保 Pishoo 能发现该账户的 DHTTP Home。
- Pishoo、gmutils 的包版本选择仍归 appliance release manifest；两个 root oneshot、
  `dhttp` 组集成和 k3s runtime provision 随同一个 `kundy` deb 发布，不再额外拆分
  `kundy-appliance-host` 包。
- service 默认启用 `NoNewPrivileges=true`、`UMask=0077`、受限写目录和明确的重启策略；hardening 以不破坏 mDNS、UDP 和 netwatcher 为前提逐项验证。
- 安装、升级和普通 remove 不删除运行用户 Home 中的 `.kundy`、`.dhttp`、正式私钥、证书或设备名字。

## 8. 开发与验证顺序

1. 调整 Cargo bin 名称并冻结首个 dhttp/h3x 兼容组合。
2. 完成 mDNS-only DHTTP endpoint、最小页面和局部测试。
3. 完成持久状态、CertServer client、CSR 和幂等激活流程。
4. 完成 gmutils/DHTTP Home identity 写入与失败回滚。
5. 接入两个宿主机 oneshot，并完成 k3s/Flux/OAuth preflight。
6. 在一次性 VM 和远程一体机验收 `kundy` deb、host installer 和 systemd 生命周期。

每个阶段只做对应 crate 的局部 `cargo check`/`cargo test`；未经确认不进行 Gateway、Chromium 或其他仓库的全量编译。

## 9. 开始编码前需要冻结的事项

- 正式采用的 dhttp/h3x 版本组合及所需 features。
- `kundy` bootstrap 证书、私钥、根 CA 的构建输入路径和到期策略。
- bootstrap UDP 端口和目标 Linux 网卡/bind pattern 策略。
- CertServer appliance API 的请求、响应和错误码 schema。
- `kundy-device-identity` ConfigMap 的正式 namespace、固定字段和 generation 规则。
