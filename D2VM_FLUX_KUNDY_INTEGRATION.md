# Kundy 与 D2VM-flux 集成实施指南

本文总结 Kundy 一体机接入 D2VM-flux 的现有实现，并作为后续真实镜像、真实
一体机和正式 Flux 集群集成时的实施基线。本文描述的是已经落在
`yuanwuzhi/D2VM-flux` `dev161` 分支中的方案，不是临时 `kubectl apply` 清单。

## 1. 当前实现基线

| 项目 | 当前值 |
| --- | --- |
| 上游仓库 | `yuanwuzhi/D2VM-flux` |
| 分支 | `dev161` |
| 当前 HEAD | `1b3c4cd fix: validate Flux substitution defaults` |
| Kundy 组件提交 | `da5ece5`、`9e3d7a2`、`d314e58`、`1b3c4cd` |
| 集群选择 | `clusters/workstation113/kustomization.yaml` |
| 运行时身份 ConfigMap | `flux-system/kundy-device-identity` |
| 逐机 OAuth Cookie Secret | `authentik/oauth2-proxy-browser-dhttp` |

四个提交的职责如下：

1. `da5ece5`：新增 Kundy appliance、DHTTP OAuth、Traefik 认证分流和
   Authentik overlay，并让 `workstation113` 选择该组件；同时让镜像同步脚本包含
   `clusters/components/**`。
2. `9e3d7a2`：把 rollout generation 明确保持为字符串，避免 Flux substitution
   后 Kubernetes annotation 类型不一致。
3. `d314e58`：增加 generation 命名的 Authentik blueprint reconciliation Job，
   解决 blueprint ConfigMap 更新不会自动写回 Provider 的问题。
4. `1b3c4cd`：让本地/CI validation 在 Flux post-build substitution 尚未执行时，
   先展开 `${VAR:=default}` 默认值再做 schema 校验。

文件级变更摘要：

| 路径 | 变更作用 |
| --- | --- |
| `clusters/workstation113/kustomization.yaml` | 让测试集群选择 `kundy-appliance` Component |
| `clusters/components/kundy-appliance/*` | 接入点、根资源 patch、LAN fallback 和 `kundy-dhttp` 子 Kustomization |
| `clusters/components/kundy-dhttp/*` | DHTTP OAuth Proxy、可信认证分流、`/oauth2` 与 `/authentik` 路由 |
| `clusters/components/kundy-authentik/*` | 在既有 public-authentik 上增加 DHTTP callback 和幂等 blueprint Job |
| `.github/workflows/sync-cluster-mirrors.yml` | 把 `clusters/components/**` 纳入每集群 Gitea 镜像 |
| `scripts/validate.sh` | 展开 Flux substitution 默认值后再执行 kubeconform |

## 2. 设计目标与边界

激活前，Kundy 只发布本地入口，DHTTP OAuth 工作负载保持 0 副本；激活后，普通
用户运行的 Kundy 只向 `/run/kundy` 写入名字和 generation 请求。root oneshot
helper 再把 CertServer 返回的永久身份写入运行时 ConfigMap，并触发 Flux
reconcile；集群中的 OAuth、Traefik 和 Authentik 资源随后使用该身份对外提供服务。

核心不变量：

- 设备名字、完整 DHTTP Host、激活状态和 generation 属于设备运行时状态，不写进
  Git，也不由 Flux 回写。
- Deployment、Service、IngressRoute、Middleware、HelmRelease patch、blueprint
  和 Job 属于 Git/Flux 管理范围。
- LAN OAuth 行为必须保持不变；DHTTP 只增加一条可信分支。
- Pishoo 必须覆盖 `X-DHTTP-Origin`，客户端不能通过自行提交该 Header 选择 DHTTP
  分支。
- Flux 不能通过 suspend、临时 apply 或删除 inventory 来“保护”设备配置。

总体关系：

```text
Kundy 激活（普通运行用户）
    │
    └─ /run/kundy/runtime-config.request
            │
            ├─ root helper 写入 ConfigMap/kundy-device-identity
            ├─ root helper 确保逐机 Cookie Secret 存在
            └─ root helper 触发一次受限 Flux reconcile
            │
            ├─ Kustomization/kundy-dhttp
            │      └─ DHTTP oauth2-proxy、认证分流、/oauth2、Authentik 路由
            └─ Kustomization/authentik
                   └─ Authentik callback、blueprint、generation Job
```

## 3. 仓库结构与 Flux 所有权

当前新增的目录：

```text
clusters/components/kundy-appliance/
  README.md
  kustomization.yaml
  flux-kustomization.yaml
  oauth2-auth-fallback.yaml

clusters/components/kundy-dhttp/
  kustomization.yaml
  oauth2-proxy.yaml
  oauth2-auth-dispatcher.yaml
  authentik-routes.yaml

clusters/components/kundy-authentik/
  kustomization.yaml
  oauth2-blueprint-reconcile.yaml
```

`clusters/workstation113/kustomization.yaml` 通过 `components` 选择
`../components/kundy-appliance`。该 Component 做两件关键事情：

1. 创建独立的 `Kustomization/kundy-dhttp`，避免根 Kustomization 在同一轮
   reconcile 中先渲染尚未替换的设备变量。
2. 把原有 `Kustomization/authentik` 的 path 切换到 `kundy-authentik` overlay，
   并让它读取相同的运行时 ConfigMap。

资源所有权必须保持如下边界：

| 资源 | 创建者 | 是否进入 Flux inventory | 说明 |
| --- | --- | --- | --- |
| `ConfigMap/flux-system/kundy-device-identity` | 宿主机 helper | 否 | 激活后持续保存设备身份 |
| `Secret/authentik/oauth2-proxy-browser-dhttp` | 宿主机 helper | 否 | 每机 Cookie Secret |
| DHTTP OAuth Deployment/Service | Flux | 是 | 由 `kundy-dhttp` 管理 |
| Traefik Middleware/IngressRoute | Flux | 是 | 认证分流与 DHTTP 入口 |
| Authentik HelmRelease/blueprint ConfigMap | Flux | 是 | 保留 LAN callback，增加 DHTTP callback |
| generation 命名 reconciliation Job | Flux | 是 | 新 generation 触发新 Job，旧 Job 可 prune |

这里的 Job 名不能只使用 generation。旧版 Kundy 在取消激活或升级迁移时可能丢失本地
generation 水位，重新激活时会复用宿主机已经见过的 generation；即使 generation 没有变化，
`DHTTP_ENABLED` 也可能从 `true` 切换为 `false`（或反向切换）。由于 Kubernetes 不允许修改
已有 Job 的 Pod template，Job 名必须同时包含 generation 和 activation state，确保启停切换
始终创建新的 Job，而不是尝试更新旧 Job。

根 Flux 使用 `prune: true` 并不会删除运行时 ConfigMap 或逐机 Secret，前提是它们
从未被 Git 清单引用、从未被某个 Flux Kustomization 作为 resource 纳入 inventory。
因此不能把这两个对象补进 `resources`，也不能在 Git 中提交每机名字、激活码或
Cookie Secret。

## 4. 运行时 ConfigMap 协议

安装器不必预先创建 ConfigMap；所有 manifest 都提供默认值，首次 Flux reconcile
仍应成功。当前 Kundy root helper 会在首次激活请求中创建它。镜像流水线如需验证
未激活状态，也可以预置以下 inert 对象，但其后仍必须由 helper 管理：

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: kundy-device-identity
  namespace: flux-system
  labels:
    reconcile.fluxcd.io/watch: Enabled
data:
  DHTTP_ENABLED: "false"
  DHTTP_CONFIG_GENERATION: "0"
  DHTTP_PARTIAL_NAME: "unconfigured.invalid"
  DHTTP_HOST: "unconfigured.invalid"
  DHTTP_DISPLAY_HOST: "unconfigured~"
  DHTTP_ORIGIN: "https://unconfigured.invalid"
  DHTTP_REDIRECT_URL: "https://unconfigured.invalid/oauth2/callback"
  DHTTP_REPLICAS: "0"
```

激活后由 helper 原子地更新固定字段，例如：

```yaml
data:
  DHTTP_ENABLED: "true"
  DHTTP_CONFIG_GENERATION: "1"
  DHTTP_PARTIAL_NAME: "tsinghua.kundy"
  DHTTP_HOST: "tsinghua.kundy.dhttp.net"
  DHTTP_DISPLAY_HOST: "tsinghua.kundy~"
  DHTTP_ORIGIN: "https://tsinghua.kundy.dhttp.net"
  DHTTP_REDIRECT_URL: "https://tsinghua.kundy.dhttp.net/oauth2/callback"
  DHTTP_REPLICAS: "1"
```

字段用途：

| 字段 | 用途 |
| --- | --- |
| `DHTTP_ENABLED` | 控制激活状态；未激活时 Job 直接退出、OAuth 副本为 0 |
| `DHTTP_CONFIG_GENERATION` | 使用 Kundy 请求中的 generation，进入 Pod template annotation 并触发 rollout |
| `DHTTP_PARTIAL_NAME` | 记录 CertServer 返回的部分身份，供排障使用 |
| `DHTTP_HOST` | 完整 `.dhttp.net` Host、精确 Header 匹配和白名单 |
| `DHTTP_DISPLAY_HOST` | `~` 短名显示和 oauth2-proxy 返回白名单 |
| `DHTTP_ORIGIN` | Authentik 公开 Origin、登录地址和转发 Header |
| `DHTTP_REDIRECT_URL` | OAuth2 Proxy 与 Authentik strict callback |
| `DHTTP_REPLICAS` | 未激活为 `0`，激活后为 `1` |

所有引用都必须继续使用 `${VAR:=default}` 形式。默认值不能是一个看似可用的
正式地址；`unconfigured.invalid` 必须保持 inert，避免未激活设备意外对外提供
OAuth 入口。

## 5. DHTTP OAuth 与 Traefik 路由

### 5.1 DHTTP OAuth Proxy

`kundy-dhttp/oauth2-proxy.yaml` 创建独立的
`oauth2-proxy-browser-dhttp` Deployment 和 Service：

- 使用独立 Cookie name：`sciflow_oauth2_proxy_dhttp`，避免与 LAN cookie 冲突。
- OAuth client ID/secret 仍从既有 `oauth2-proxy-browser` Secret 读取。
- Cookie secret 从逐机 Secret `oauth2-proxy-browser-dhttp` 读取。
- `DHTTP_REPLICAS` 控制副本数。
- `DHTTP_CONFIG_GENERATION` 进入 Pod template annotation；字符串类型不可省略。
- 登录、callback 和 whitelist 使用完整 `.dhttp.net` 地址；`~` 只作为返回白名单
  和界面显示名，不用于 Authentik Provider strict callback。

### 5.2 认证分流

原有 `sciflow/oauth2-forward-auth` Middleware 被改为请求 Traefik 内部路径：

```text
/__kundy/oauth2/auth
```

该路径有两条精确分支：

1. `X-DHTTP-Origin == DHTTP_HOST`：转发到
   `oauth2-proxy-browser-dhttp`。
2. 其他情况：转发到原有 LAN `oauth2-proxy-browser`。

分流路由自身不能挂 `ForwardAuth`，否则会递归。内部路径使用
`replacePath: /oauth2/auth`，避免与公开 `/oauth2` callback 路由竞争。

DHTTP 分支只允许精确匹配 Pishoo 写入的 `X-DHTTP-Origin`，不能使用客户端可控的
Host 后缀通配。Pishoo 必须在请求进入集群前覆盖该 Header。

### 5.3 固定路由与动态工作区

当前 SciFlow 固定受保护前缀为：

```text
/sciflow/gateway
/sciflow/agent
/sciflow/policy
/sciflow/storage
/sciflow/orchestrator
/sciflow/images
/sciflow/operations
/sciflow/reporting
```

这些路由和 controller 动态创建的 `/instances/...` 工作区都通过
`browser-app-authz` 或 `browser-platform-admin-authz` chain 间接使用同一个
`oauth2-forward-auth`。因此不复制静态 DHTTP 路由，而是在共享 Middleware 入口分流，
可以覆盖未来动态生成的 Service/Ingress。

`/oauth2` 与 `/authentik` 仍需要独立的 DHTTP exact-header IngressRoute，以便使用
正确的 callback 和 public-origin Header。

## 6. Authentik callback reconciliation

仅更新 blueprint ConfigMap、Secret 或 rollout Authentik worker/server，不保证
Authentik 2026.5.0 把新 callback 写入数据库 Provider。为此 overlay 创建：

```text
Job/authentik-oauth2-blueprint-${DHTTP_CONFIG_GENERATION}-${DHTTP_ENABLED}
```

Job 行为：

- `DHTTP_ENABLED` 不是 true 时打印跳过并成功退出。
- 激活后执行 `ak apply_blueprint /blueprints/oauth2-proxy-browser.yaml`。
- 只挂载 OAuth blueprint ConfigMap，并从现有 Secret 读取 Authentik/OAuth 配置。
- `automountServiceAccountToken: false`，不依赖 Kubernetes API 权限。
- Job 名包含 generation 和 enabled 状态；任一变化时创建新 Job，旧 Job 可由 Flux prune，
  避免启停切换修改不可变的 Job Pod template。即使旧版 Kundy 在启停切换时复用同一个
  generation，`...-true` 与 `...-false` 也必须是两个不同的 Job 名。
- Job Complete 只能说明命令结束，不能证明 Provider 已存储正确 callback。

Blueprint 必须同时保留原 LAN callback，并增加当前设备的 strict DHTTP callback：

```text
https://<完整设备名>.dhttp.net/oauth2/callback
```

验收时必须通过 Authentik API 或数据库读取 Provider 的实际 redirect URI，不能只看
Pod Ready、HelmRelease Ready 或 Job Complete。

## 7. 真实镜像集成步骤

以下顺序适用于将当前 Kundy 二进制和宿主机 helper 集成到真实一体机镜像。

### A. 固化 Flux 来源

1. 确认量产使用的 D2VM-flux 分支/commit，而不是临时工作区或现场导出的 tarball。
2. `workstation113` 只是当前 live 测试的选择点。每个真实设备对应的
   `clusters/<cluster>/kustomization.yaml` 都必须显式选择
   `../components/kundy-appliance`；不能假设修改 `workstation113` 会自动覆盖其他镜像。
3. 如果确认所有同一基线的设备都必须启用 Kundy，也可以在评审后上移到公共 overlay，
   但必须先验证不会影响不具备 Kundy/Pishoo/helper 的旧设备。
4. 如果使用 Gitea/内网镜像，确认同步 workflow 包含
   `clusters/components/**`，否则镜像仓库会缺失 Kundy Component。
5. 在目标集群的 `GitRepository/flux-system` 中确认 revision 与预期 commit 一致。

### B. 镜像预置宿主机依赖

镜像需要预置：

- `kundy` 二进制和 systemd unit。
- Pishoo 及其 systemd unit。
- `gmutils`，尤其是 `genmeta` helper。
- 可写的运行用户 Home、`~/.kundy` 和标准 `~/.dhttp` 目录权限。
- `/run/kundy` 文件协议对应的两个 systemd path/oneshot unit。
- 能执行受限 k3s 操作的 root helper；当前由同一个二进制的
  `kundy host runtime-config-apply` 和 `kundy host pishoo-reload` 提供。
- 不要求固定用户名，但安装时必须明确实际运行 Kundy/Pishoo 的既有普通用户。

Kundy 的 `config/kundy.toml` 通过 `include_str!` 编译进二进制，不是部署后的运行时
配置。量产构建必须在编译前确认 CertServer 正式地址和默认激活域；安装后修改磁盘上
的 TOML 不会改变行为。出厂 DHTTP 证书、私钥和页面资源同样内嵌，镜像运行时只需要
安装经过发布校验的目标架构 `kundy` 二进制。

从源码目录安装时的开发入口为：

```bash
sudo ./packaging/host/install-kundy-host.sh \
  --binary /path/to/release/kundy \
  --user <实际运行用户>
```

量产镜像使用 `kundy` deb 时，安装包后通过 `sudo kundy setup --user <实际运行用户>`
完成同样的宿主机配置；deb 已经包含内部 setup helper 和 systemd units。

安装脚本要求 Pishoo、gmutils、k3s 和目标普通用户已经存在；它不会创建专用
`kundy-bootstrap` 用户，也不会删除该用户 Home 中已有的 `.kundy` 或 `.dhttp`。

当前 helper 会在首次 runtime-config 请求时惰性创建逐机 Cookie Secret；已存在且
合法时复用，不得在更新或重装时重生成。Cookie Secret 的原始值必须是 16、24 或
32 字节；例如 `openssl rand -hex 16` 生成的 32 字节 ASCII 值可供 oauth2-proxy
使用。激活码不写入 Flux Secret、Git 或 Kubernetes ConfigMap。

### C. 激活后的 helper 操作

Kundy 完成 CertServer 激活并持久化恢复凭据后，当前自动链路按以下顺序执行：

1. Kundy 将部分永久名字和已经持久化的 generation 原子写入
   `runtime-config.request`；请求不包含激活码、私钥、CSR、路径或 Kubernetes 对象名。
2. `kundy-runtime-config-apply.path` 触发 root oneshot。helper 校验名字和 generation，
   拒绝 generation 回退或替换已绑定名字。
3. helper 确保逐机 Cookie Secret 存在，然后创建/更新
   `kundy-device-identity` 的固定字段；完整 Host、Origin 和 callback 由 helper 根据
   部分名字计算，不能从请求注入。
4. helper 给 `authentik` 和 `kundy-dhttp` 两个固定 Flux Kustomization 写入本次
   `reconcile.fluxcd.io/requestedAt`，并把 `cluster-settings/HOST` 返回给 Kundy。
5. Kundy 以普通运行用户权限生成正式 identity 的 `server.conf`，固定反代
   `127.0.0.1:80`，覆盖公开 Origin 相关 Header，再写入 `pishoo-reload.request`。
6. `kundy-pishoo-reload.path` 触发第二个 root oneshot。helper 确认两个
   Kustomization 未暂停、`lastHandledReconcileAt` 已处理当前 Kundy generation 且
   `Ready=True`（不依赖可能被上层 GitOps 清除的临时 requestedAt annotation），确认 identity、
   `ssl` 目录和 `server.conf` 存在，通过 Pishoo 配置检查后 reload，并确认服务 active。
7. 两个 result 都匹配当前 generation 后，Kundy 才把 runtime 状态报告为 `ready`。

Provider 数据库中的 callback、OAuth Deployment 副本以及真实业务路由属于发布验收
项，目前不作为每次激活 helper 的判定条件；真实镜像发布流水线必须另外检查它们。

ConfigMap 的 watch label 只会请求新的 reconcile；如果 Kustomization 被
`spec.suspend: true`，watch 不会自动恢复它。正式 helper 必须保证 Flux 正常运行，
或使用经过授权、范围明确的单次 reconcile 操作，不能依赖长期 suspend。

### D. 真实镜像首次启动

首次启动应满足：

- Kundy 本地入口可访问，但 `DHTTP_REPLICAS=0`。
- 未激活默认值不会生成有效的公开 DHTTP callback。
- Flux 根 Kustomization、`authentik` 和 `kundy-dhttp` 均能正常 reconcile。
- 激活前不创建或不启用 DHTTP OAuth Pod。

激活后再验证 `DHTTP_CONFIG_GENERATION` 从 0 到 1（或当前值加一），确认两个 Pod
template 均发生 rollout，而不是只更新 ConfigMap。

## 8. 验证与验收清单

### 静态验证

在 D2VM-flux 工作区执行：

```bash
./scripts/validate.sh
```

依赖版本以脚本头部为准：`yq v4.48`、`kustomize v5.7`、`kubeconform v0.7`。
脚本会下载 Flux schema，验证 YAML、Kustomization 输出，并在校验前展开默认替换值。
独立 `kind: Component` 不应直接单独 build；应通过目标 cluster overlay 验证。

另外执行目标集群渲染并检查：

```bash
kustomize build clusters/workstation113 --load-restrictor=LoadRestrictionsNone
```

确认输出不存在 `kosumi.kundytest`、测试 Cookie Secret、测试 callback 或其他设备
硬编码。

### 集群验收

常用只读检查命令：

```bash
sudo k3s kubectl -n flux-system get configmap kundy-device-identity -o yaml
sudo k3s kubectl -n flux-system get kustomization authentik kundy-dhttp
sudo k3s kubectl -n authentik get deployment oauth2-proxy-browser-dhttp
sudo k3s kubectl -n authentik get jobs -l app.kubernetes.io/name=authentik-oauth2-blueprint
sudo systemctl status kundy.service pishoo.service \
  kundy-runtime-config-apply.path kundy-pishoo-reload.path
```

命令输出可能包含设备公开名字，但不应包含激活码、私钥或 Cookie Secret 明文；排障
记录不要导出 Secret YAML。

1. 未激活：ConfigMap 缺失或 `DHTTP_ENABLED=false` 时，Flux 仍 Ready，DHTTP OAuth
   副本为 0。
2. 激活：ConfigMap 写入后，`kundy-dhttp` 和 `authentik` 都 Ready，OAuth 副本为 1。
3. Generation：ConfigMap generation 增加后，DHTTP OAuth 和 Authentik rollout
   annotation 更新。
4. 启停兼容：使用同一个测试 generation 分别展开 `DHTTP_ENABLED=true` 和 `false`，
   预期 Job 名分别为 `authentik-oauth2-blueprint-<generation>-true` 和
   `authentik-oauth2-blueprint-<generation>-false`；两者不能相同，且不得通过修改已有
   Job 的 Pod template 来实现切换。
5. Provider：通过 Authentik API/数据库确认 LAN callback 保留、DHTTP strict callback
   正确，且没有旧设备 callback。
6. 路由：DHTTP 控制台、`/oauth2`、`/authentik` 和八个 SciFlow 固定前缀均可用。
7. 动态工作区：至少启动一个 code-server 和一个 KasmVNC 实例，验证真实
   `/instances/...` 路由走 DHTTP OAuth。
8. LAN 回归：LAN nip.io 入口仍使用原 OAuth proxy、cookie 和 callback。
9. 持久性：提交无关 Git 更新、重启 k3s、删除并重建相关 Pod 后，设备名字和
   ConfigMap data 不变。
10. 安全：Flux inventory 不包含 `kundy-device-identity` 和逐机 Cookie Secret；
   客户端伪造 `X-DHTTP-Origin` 不能切换到 DHTTP 分支。

## 9. 回滚与故障处理

### 代码回滚

优先回滚 D2VM-flux 到上一个已知可用 commit，再让 Flux 正常 reconcile。不要先手工
删除 Runtime ConfigMap；它是设备身份的持久状态，不是发布产物。

### 只关闭 DHTTP 集成

如果需要保留 LAN 服务而暂时关闭 DHTTP，可将 `DHTTP_REPLICAS` 设为 `0`、
`DHTTP_ENABLED` 设为 `false`，递增 generation 并完成 reconcile。不要删除共享
`oauth2-forward-auth` 的分流资源，除非同时确认所有固定和动态 LAN 路由已恢复原地址。

### 常见故障定位

| 现象 | 优先检查 |
| --- | --- |
| ConfigMap 更新后没有变化 | Kustomization 是否 suspend；watch label、generation、`substituteFrom` namespace |
| OAuth Pod 没启动 | `DHTTP_REPLICAS`、逐机 Cookie Secret、Kustomization build 输出 |
| 登录后回调失败 | 完整 `.dhttp.net` callback、Origin/Host、Authentik Provider 实际数据库值 |
| LAN 入口也失败 | `oauth2-forward-auth` fallback 路由和原 LAN OAuth Service |
| 动态工作区 403/登录循环 | controller 生成的 chain 是否仍引用共享 Middleware；Pishoo Header 是否被覆盖 |
| Job Complete 但 callback 未变 | 通过 Authentik API/数据库检查 Provider，确认 generation Job 使用了最新 blueprint |
| 镜像集群缺少 Component | Gitea mirror workflow 是否包含 `clusters/components/**` |

## 10. 当前仍需正式环境补验的事项

- 当前 helper、两个 systemd path/oneshot、Flux reconcile 和 Pishoo reload 已在
  现有测试一体机上完成 live 验证；尚未验证的是它们作为真实镜像预置内容时的首次
  启动、升级和 repair 流程。
- 当前已验证静态清单、Authentik reconciliation 和永久名字的 AnySee 访问；动态
  `/instances/...` 仍需真实
  code-server、KasmVNC 工作区分别验证。
- 需要在正式运行的 Flux 集群中确认根 Flux、Authentik 和 Kundy 子 Kustomization
  均处于正常 reconcile，而不是沿用测试阶段的暂停状态。
- 需要确认正式镜像的运行用户对 k3s helper、`~/.kundy`、`~/.dhttp` 和 systemd
  unit 的权限边界，并避免把 sudo 权限扩大为整个集群管理员权限。

完成上述补验后，才可以把当前 `dev161` 方案作为真实量产镜像的默认集成基线。

## 参考资料

- `DEVELOPMENT_PLAN.md`：Kundy 激活和宿主机运行时设计。
- `HOST_RUNTIME_PROTOCOL.md`：Kundy 与宿主机 helper 的 generation/reconcile 协议。
- D2VM-flux `clusters/components/kundy-appliance/README.md`：Flux 侧运行时对象、
  Authentik 和认证分流约束。
- yitiji 工作区的 `kundy-flux-artifact-audit.md`：初始 Flux artifact 审计和 live
  验证记录。
- yitiji 工作区的 `kundy-flux-upstream-change-request.md`：上游仓库最小变更请求与
  验收要求。
