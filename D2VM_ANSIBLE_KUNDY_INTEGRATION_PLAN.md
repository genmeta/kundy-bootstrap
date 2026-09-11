# Kundy 集成 D2VM-Ansible 实施计划

本文档定义 Kundy 与 `/Users/x/code/yuanwuzhi/D2VM-Ansible` 的后续集成方式。
目标是让 Ansible 负责宿主机准备和软件发布，让 Kundy 负责运行时身份生命周期，
让 Flux 负责 Kubernetes 期望状态。除非明确更新本计划，不应让三个系统互相越权。

## 1. 目标和非目标

### 1.1 目标

- 使用 D2VM-Ansible 在目标宿主机安装和升级 Kundy deb。
- 明确指定一个已经存在的普通用户作为 Kundy 运行用户。
- 在安装前完成 Pishoo、gmutils、k3s、systemd 和 `dhttp` 组检查。
- 通过公开入口 `sudo kundy setup --user <user>` 完成宿主机 systemd 配置。
- 支持 fresh、migration、upgrade、re-apply 和 airgap 场景。
- 让每个 inventory 可以独立决定是否启用 Kundy，以及使用哪个 Flux 集群。
- 在不写入激活码、私钥、Cookie Secret 和设备名字的前提下完成自动化部署。

### 1.2 非目标

D2VM-Ansible 不负责：

- 执行 `kundy activate` 或 `kundy deactivate`。
- 写入 `/run/kundy` 下的运行时请求文件。
- 直接写入 `kundy-device-identity` ConfigMap 或 OAuth Cookie Secret。
- 直接执行 Kundy 的 k3s/Flux/Pishoo root helper。
- 复制 Kundy 的 systemd unit 到目标机；这些文件由 deb 包管理。
- 在目标机编译 Rust 或构建 deb。

## 2. 三方职责边界

| 组件 | 负责内容 | 不负责内容 |
| --- | --- | --- |
| D2VM-Ansible | 主机预检、K3s 部署、Pishoo/gmutils 前置、deb 安装、`kundy setup`、结果验证 | 激活身份、运行时 ConfigMap、Flux 运行时 reconcile |
| Kundy deb | `/usr/bin/kundy`、内部 setup helper、主服务模板、固定 path/oneshot units | 创建专用用户、选择未知用户、安装 K3s/Pishoo |
| Kundy 服务 | 普通用户运行的本地页面、激活和持久状态 | root 权限、kubeconfig、sudo |
| Kundy root oneshot | 固定请求协议、k3s/ConfigMap/Flux/Pishoo 操作 | 任意命令、任意路径、任意 Kubernetes 对象 |
| D2VM-flux | Deployment、Service、IngressRoute、Authentik、OAuth 和 HelmRelease | 每台机器的设备名字、激活码、Cookie Secret |

关键原则：Ansible 可以调用公开的 `kundy setup`，但不应调用
`/usr/libexec/kundy/setup`，也不应绕过 Kundy 协议修改集群运行时资源。

## 3. D2VM-Ansible 当前形态和适配点

当前仓库的主要入口是：

- `playbooks/fresh/site.yml`：基础环境、GPU、K3s、工具、Secret、Flux。
- `playbooks/migrate/site.yml`：备份、旧 K8s/Docker 清理、迁移、K3s、工具、Flux。
- `roles/flux`：安装 Flux、生成 GitRepository/Kustomization、配置 deploy key。
- `roles/d2vm_secrets`：向 `haios` 注入 SSH 和 kubeconfig Secret。
- `roles/d2vm_services`：把开发服务内容同步到 NFS。
- `inventory/hosts-*`：同时承载拓扑、K3s 参数、离线目录和 Flux 仓库选择。

适配时需要注意：

1. 有些 inventory 使用 root SSH 登录，有些使用普通用户登录；`ansible_user` 不能
   自动作为 Kundy 运行用户。
2. 当前 Ansible 主要面向 K3s server 和集群基础设施，Kundy 应只部署到明确标记的
   宿主机，通常是 `server` 中运行 Pishoo 的节点。
3. 当前仓库没有完整管理 Pishoo 和 gmutils 的 role；第一阶段应将它们视为镜像或
   appliance release 的前置依赖，并由 Kundy role 做 preflight。
4. 不同 inventory 使用不同 Flux 仓库和 cluster path，不能假定所有集群都已经包含
   Kundy 的 DHTTP 组件。

## 4. 推荐的 Ansible 目录结构

在 D2VM-Ansible 中新增一个独立 role 和 playbook：

```text
roles/kundy/
  defaults/main.yml
  tasks/main.yml
  tasks/preflight.yml
  tasks/install.yml
  tasks/setup.yml
  tasks/verify.yml
  handlers/main.yml

playbooks/kundy.yml
```

职责建议如下：

- `preflight.yml`：只读检查操作系统、架构、普通用户、Home、依赖程序和 Flux 前置。
- `install.yml`：分发并校验 deb，使用 Ansible `apt` 模块安装或升级。
- `setup.yml`：调用 `/usr/bin/kundy setup --user ...`，不复制 unit 文件。
- `verify.yml`：检查 dpkg、systemd、运行用户、Home、组和服务状态。
- `handlers/main.yml`：只保留必要的包安装后处理；不要自行重写 Kundy service。

`playbooks/kundy.yml` 应只针对启用了 `kundy_enabled` 的主机，例如：

```yaml
- name: Configure Kundy host integration
  hosts: kundy_hosts
  become: true
  roles:
    - kundy
  tags: [kundy]
```

第一阶段不建议把 role 直接写入所有 `k3s_cluster`。可以在 inventory 中显式建立：

```yaml
kundy_hosts:
  hosts:
    192.168.3.185:
      kundy_enabled: true
```

或者在现有 `server` 主机上设置 `kundy_enabled: true`，但必须保证该主机确实运行
Pishoo 和 Kundy 所需的本地依赖。

## 5. Inventory 变量契约

建议在 `roles/kundy/defaults/main.yml` 中提供默认值，在具体 inventory 中覆盖：

```yaml
kundy_enabled: false
kundy_runtime_user: ""
kundy_deb_path: ""
kundy_deb_url: ""
kundy_deb_sha256: ""
kundy_setup_enabled: true
kundy_verify_enabled: true
kundy_setup_on_package_change: true
kundy_setup_force: false
kundy_flux_runtime_enabled: false
kundy_expected_flux_kustomizations:
  - kundy-dhttp
  - authentik
```

变量规则：

- `kundy_runtime_user` 必须显式填写，不能为空，也不能是 `root`。
- 不默认使用 `ansible_user`，避免 root SSH 登录时错误地把 Kundy 配成 root。
- `kundy_deb_path` 和 `kundy_deb_url` 二选一；airgap 使用 `kundy_deb_path`。
- 生产环境必须提供 `kundy_deb_sha256`，没有 checksum 时只允许开发环境继续。
- `kundy_flux_runtime_enabled` 只有在对应 Flux 仓库已经部署 Kundy 组件时才设为 true。
- 不在 inventory 中保存激活码、私钥、Cookie Secret、设备名字或 Kubernetes token。

可选的 release 级变量：

```yaml
kundy_release_version: "0.1.0"
kundy_release_channel: "stable"
kundy_artifact_arch: "amd64"
```

其中 `kundy_artifact_arch` 最好由 Ansible facts 计算，而不是手写：

| `ansible_architecture` | deb 架构 |
| --- | --- |
| `x86_64` | `amd64` |
| `aarch64` | `arm64` |
| `armv7l` / `armv7` | `armhf` |
| `i386` / `i686` | `i386` |

## 6. 部署顺序

### 6.1 主机初始化顺序

完整 appliance 的推荐顺序：

```text
base
  -> K3s
  -> CLI tools
  -> Pishoo/gmutils/k3s preflight
  -> Kundy deb install
  -> kundy setup --user <runtime-user>
  -> D2VM secrets
  -> Flux
  -> Kundy/Flux/Pishoo final verification
```

其中 deb 安装和 setup 可以拆成两个阶段：

- 安装阶段只安装文件，不启动 Kundy；
- setup 阶段在 Pishoo、gmutils、k3s 已就绪后生成主 service 并启用固定 units。

这样可以在安装包失败时区分“制品/依赖问题”和“宿主机 service 配置问题”。

### 6.2 Fresh 流程

在 `playbooks/fresh/site.yml` 中增加一个可选阶段：

```yaml
- name: "Phase: Configure Kundy Host"
  ansible.builtin.import_playbook: ../kundy.yml
  tags: [kundy]
```

建议第一阶段把该阶段放在 K3s、工具和 Pishoo/gmutils preflight 之后。Flux 可以在
Kundy setup 前或后部署，但最终验证必须在 Flux 完成后执行。

### 6.3 Migration 流程

迁移场景中不要在旧 K8s/Docker 清理前安装 Kundy。推荐顺序：

```text
备份
  -> 清理旧 K8s/Docker
  -> OS/GPU 处理
  -> K3s
  -> 数据迁移
  -> Pishoo/gmutils preflight
  -> Kundy deb install/setup
  -> Flux
```

迁移 role 必须保证 Kundy 不会触碰旧用户 Home 中已有的 `.kundy`、`.dhttp`、证书、
私钥和设备名字。

### 6.4 维护和重复执行

日常维护使用：

```bash
ansible-playbook -i inventory/hosts-<cluster> playbooks/kundy.yml --tags kundy
```

`kundy setup` 本身是幂等入口。Role 应在以下情况执行 setup：

- 首次安装，`/etc/systemd/system/kundy.service` 不存在；
- Kundy deb 发生升级；
- `kundy_runtime_user` 发生变化；
- 显式设置 `kundy_setup_force=true`。

普通重复运行只做只读验证，避免每次 Ansible 执行都无条件重启 Kundy。

## 7. Role 任务设计

### 7.1 Preflight

`preflight.yml` 使用 Ansible facts 和模块完成以下检查：

1. 操作系统属于 Debian/Ubuntu 系列，存在 systemd 和 `dpkg`。
2. 当前主机架构在 deb 发布矩阵内。
3. `kundy_runtime_user` 存在、UID 非 0、Home 是绝对路径且目录存在。
4. `dhttp` 组存在，运行用户可以加入该组。
5. `/usr/bin/genmeta`、`/usr/bin/pishoo`、`/usr/local/bin/k3s` 存在且可执行。
6. `pishoo.service` 和相应的 K3s service 已安装或可查询。
7. deb 制品存在，架构和 checksum 与目标主机匹配。
8. `kundy_flux_runtime_enabled=true` 时，集群 kubeconfig 和 Flux CLI 可用。

预检失败应在 apt 安装前终止，并明确指出缺少的依赖。不要让 setup 脚本运行到一半
才暴露基础环境问题。

### 7.2 制品分发和安装

推荐使用以下流程：

1. 从 CI 或制品目录取得 deb，不在目标机编译。
2. 使用 `copy` 或 `get_url` 将 deb 放到临时目录。
3. 对下载文件执行 sha256 校验并用 `assert` 比对 `kundy_deb_sha256`。
4. 使用 `ansible.builtin.apt` 安装本地 deb。
5. 记录包版本、架构和制品 checksum。

不要把 `.deb`、`.buildinfo` 或 `.changes` 提交到 D2VM-Ansible Git 仓库。airgap
制品应由 appliance release 目录或外部制品存储统一管理。

### 7.3 Setup

setup task 只执行：

```bash
/usr/bin/kundy setup --user <kundy_runtime_user>
```

不要在 Ansible 中复制以下文件：

```text
/usr/libexec/kundy/setup
/usr/lib/kundy/systemd/kundy.service.in
/usr/lib/systemd/system/kundy-*.path
/usr/lib/systemd/system/kundy-*.service
```

这些文件的来源和生命周期由 deb 负责。setup 成功后，预期生成：

```text
/etc/systemd/system/kundy.service
```

并启用：

```text
kundy.service
kundy-runtime-config-apply.path
kundy-pishoo-reload.path
```

Ansible 不应额外执行 `systemctl enable`、`systemctl restart pishoo` 或直接写 unit，
否则会产生两套生命周期逻辑。

### 7.4 Verify

验证至少包含：

```bash
dpkg-query -W -f='${Package} ${Version} ${Architecture}\n' kundy
systemctl is-enabled kundy.service
systemctl is-enabled kundy-runtime-config-apply.path
systemctl is-enabled kundy-pishoo-reload.path
systemctl is-active kundy.service
systemctl is-active kundy-runtime-config-apply.path
systemctl is-active kundy-pishoo-reload.path
systemctl show kundy.service -p User -p Environment -p ExecStart
id <kundy_runtime_user>
```

验证结果应断言：

- `User` 是 `kundy_runtime_user`，不是 root；
- `Environment=HOME=` 指向该用户真实 Home；
- 用户属于 `dhttp` 组；
- 三个需要运行的 unit 都处于 enabled/active；
- 现有 `.kundy`、`.dhttp`、证书和私钥仍存在。

## 8. Flux 集成门槛

Ansible 的 `roles/flux` 目前支持多个仓库和 cluster path，因此不能只凭仓库名字判断
是否支持 Kundy。开启 `kundy_flux_runtime_enabled` 前，应在对应 D2VM-flux 仓库确认：

- 集群选择了 Kundy appliance Component；
- 存在 `kundy-dhttp` Kustomization；
- Authentik Kustomization 使用 Kundy overlay；
- Flux 中有 `cluster-settings` ConfigMap；
- 运行时 ConfigMap 和 Cookie Secret 没有被 Flux inventory 管理；
- 未激活默认值可以成功渲染，DHTTP 副本为 0；
- 激活后 generation、身份 Host 和 callback substitution 可用。

建议在 Kundy role 中增加可选只读检查，而不是由 Ansible 创建运行时对象：

```bash
kubectl -n flux-system get kustomization kundy-dhttp
kubectl -n flux-system get kustomization authentik
kubectl -n flux-system get configmap cluster-settings
```

如果目标 Flux 仓库尚未接入 Kundy，role 可以安装 deb，但必须报告“宿主机已就绪、
Flux runtime 未启用”，不能把整套集成标记为通过。

## 9. 发布和 airgap 流程

推荐将构建和部署解耦：

```text
Kundy CI/开发机使用 Podman 构建 deb
  -> 发布 deb、版本、架构、sha256
  -> appliance artifact 目录或制品存储
  -> D2VM-Ansible copy/get_url 分发
  -> 目标主机 apt 安装
  -> kundy setup
```

构建端使用 Kundy 已有的：

```bash
CONTAINER_ENGINE=podman ./scripts/build-deb.sh \
  --target x86_64-unknown-linux-gnu
```

D2VM-Ansible 不应依赖 Docker daemon，也不应假设目标机有 Podman。Podman/Docker 只
属于构建端；目标 Debian/Ubuntu 主机只需要 apt、systemd 以及 Kundy 的运行时依赖。

airgap 发布目录建议同时包含：

```text
kundy_<version>_<arch>.deb
kundy_<version>_<arch>.sha256
release-manifest.yml
k3s airgap packages
other appliance packages
```

release manifest 至少记录 Kundy 版本、deb 架构、sha256、K3s 版本、Pishoo/gmutils
版本和对应 Flux 仓库 commit，避免只升级一个组件造成协议不匹配。

## 10. 安全和幂等要求

- `kundy_runtime_user` 必须是明确的既有普通用户，禁止隐式选择 root。
- deb 安装和 setup 使用 `become: true`；普通 Kundy 服务仍以非 root 用户运行。
- Ansible 日志不得打印激活码、私钥、Secret 内容或完整 kubeconfig。
- 制品必须做 checksum 校验；生产环境拒绝未签名或未校验的 deb。
- 运行时 ConfigMap、Cookie Secret 和请求文件不由 Ansible 管理。
- setup 失败时不自动删除用户 Home、证书、私钥或已有运行态。
- 升级和普通 remove 不删除 `.kundy`、`.dhttp` 和设备身份文件。
- `--check` 模式只执行预检和只读验证，不运行 setup 或服务变更。
- 不在 Ansible 中引入第二套定时 reconcile 或 Pishoo reload 逻辑。

## 11. 失败处理和回滚

### 11.1 安装前失败

依赖、架构、用户、checksum 或 Flux 前置不满足时，在安装 deb 前失败，保留清晰的
宿主机修复提示。

### 11.2 deb 安装成功但 setup 失败

保留已安装的 deb，不删除旧 service 或用户数据；输出：

- setup 的退出码；
- `systemctl status kundy.service`；
- `journalctl -u kundy.service`；
- 缺失的 Pishoo/gmutils/k3s 依赖。

修复依赖后重新执行 `--tags kundy` 或设置 `kundy_setup_force=true`。

### 11.3 升级回滚

回滚使用上一份经过 checksum 校验的 deb：

```bash
sudo apt install ./kundy_<previous-version>_<arch>.deb
sudo kundy setup --user <existing-user>
```

回滚不应删除运行用户 Home 或清理 CertServer 绑定。若运行协议发生不兼容，必须
同时回滚对应的 Flux component commit 和 Pishoo/gmutils release。

## 12. 验收矩阵

### 12.1 静态检查

在 D2VM-Ansible 控制节点执行：

```bash
ansible-playbook -i inventory/hosts-<cluster> playbooks/kundy.yml --syntax-check
ansible-playbook -i inventory/hosts-<cluster> playbooks/kundy.yml --list-tasks
ansible-playbook -i inventory/hosts-<cluster> playbooks/kundy.yml --check --diff
```

同时检查 Kundy deb：

```bash
dpkg-deb --info kundy_*.deb
dpkg-deb --contents kundy_*.deb
```

### 12.2 主机测试

至少覆盖：

| 场景 | 预期 |
| --- | --- |
| Debian/Ubuntu x86_64 | deb 安装、setup、三类 unit 均成功 |
| Debian/Ubuntu arm64 | 架构选择和服务行为成功 |
| Ansible root SSH | Kundy 仍使用显式普通用户 |
| Ansible 普通用户 SSH + become | setup 仍由 root 执行 |
| 首次安装 | 生成正确的 `kundy.service` |
| 重复执行 | 不破坏 Home，服务保持正常 |
| 用户切换 | 新 service 使用新用户，旧用户数据不删除 |
| deb 升级 | setup 可重新渲染，运行态文件保留 |
| setup 失败 | 不删除用户数据，可修复后重试 |
| 普通 remove | 不删除 `.kundy`、`.dhttp`、证书和私钥 |

### 12.3 运行时端到端测试

在 D2VM-flux 已接入的测试集群上执行：

1. 安装 deb 并运行 setup。
2. 确认 Kundy 本地页面可访问。
3. 执行 `kundy activate`。
4. 确认 `/run/kundy/runtime-config.request` 被处理。
5. 确认固定 ConfigMap 更新、两个 Flux Kustomization reconcile 完成。
6. 确认 Pishoo reload oneshot 成功、DHTTP identity 可发现。
7. 执行 `kundy deactivate`，确认 DHTTP 被禁用且 Pishoo reload 成功。
8. 重启 Kundy、Pishoo 和 k3s，确认 generation 和持久状态可恢复。

## 13. 分阶段实施路线

### 阶段 1：单机验证

- 在 D2VM-Ansible 新增 `roles/kundy` 和 `playbooks/kundy.yml`。
- 只支持一个 x86_64 测试 inventory。
- 使用本地 checksum deb，不接入自动发布系统。
- 验证 root SSH、普通运行用户和重复 setup。

完成标准：主机服务和四个 deb 提供的 unit 行为稳定，Ansible 可重复执行。

### 阶段 2：接入 fresh/migrate

- 在 `fresh/site.yml` 和 `migrate/site.yml` 添加 `kundy` 可选阶段。
- 保持 `kundy_enabled: false` 默认值。
- 补齐 Pishoo/gmutils preflight 和架构映射。
- 将安装、setup、verify 拆成可单独执行的 tags。

完成标准：不启用 Kundy 的既有集群行为不变，启用后不会改变 K3s/Flux 现有流程。

### 阶段 3：制品和 airgap

- Kundy CI 使用 Podman 生成多架构 deb。
- 发布 checksum 和 release manifest。
- D2VM-Ansible 支持在线 URL 和离线 artifact 两种来源。
- 在至少一个 arm64 节点完成安装验收。

完成标准：无 Docker daemon 的构建环境和无外网的目标环境都能完成部署。

### 阶段 4：Flux runtime 集成

- 为每个目标 Flux 仓库确认 Kundy appliance Component。
- 在 inventory 中逐集群开启 `kundy_flux_runtime_enabled`。
- 增加只读 Flux 前置检查和最终 runtime 验证。
- 完成 activate/deactivate/upgrade/reboot 端到端测试。

完成标准：Ansible 只负责安装和验证，身份变更全部通过 Kundy 协议完成。

### 阶段 5：量产发布

- 将 Kundy、Pishoo、gmutils、K3s 和 Flux commit 纳入同一 appliance release manifest。
- 将 deb 放入镜像或 airgap artifact 的正式供应链。
- 建立升级、回滚和普通 remove 的验收记录。
- 在量产镜像中固定执行 `kundy setup --user <runtime-user>` 的来源和时机。

完成标准：新装、迁移、升级、回滚和重新激活均有可重复的自动化路径。

## 14. 第一批待办事项

建议实际开始编码时按以下顺序执行：

1. 在 D2VM-Ansible 新增 `kundy` role 的 defaults 和 preflight。
2. 确定第一个测试 inventory 的 `kundy_runtime_user`、deb 来源和 checksum。
3. 新增 `playbooks/kundy.yml`，只做安装、setup、verify。
4. 用 `--syntax-check`、`--list-tasks` 和 `--check` 验证 playbook。
5. 在一次性 Debian/Ubuntu VM 安装并重复执行两次。
6. 将 `kundy` 阶段以默认关闭方式接入 fresh/migrate。
7. 确认目标 Flux 仓库后，再开启 `kundy_flux_runtime_enabled`。
8. 最后增加 CI artifact 发布和 airgap 分发，不要在 role 中引入编译逻辑。
