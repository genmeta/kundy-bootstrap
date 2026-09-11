# Kundy 宿主机运行态协议

`kundy` 以非 root 用户运行，不安装 Kubernetes client，也不持有 kubeconfig。
激活成功后，它只通过 `/run/kundy` 下的四个文件与安装器安装的
root helper 通信。

## 目录权限

`kundy.service` 通过 `RuntimeDirectory=kundy` 创建目录。以下文件按请求动态产生，
不是预置的空文件：

```text
/run/kundy/
  runtime-config.request
  runtime-config.result
  pishoo-reload.request
  pishoo-reload.result
```

安装器把目录所有权授予实际运行 Kundy 的既有宿主机账户，并限制其他普通用户访问；
不要求固定用户名或专用组。该账户需要能够创建或原子替换 `*.request`，并读取
`*.result`；helper 以 root 身份运行，只处理这四个固定文件名，并把结果写成运行
账户可读。请求和响应都必须限制为小型 JSON，禁止把激活码、私钥、CSR 或
kubeconfig 写入此目录。

## 请求格式

两个请求使用同一 schema：

```json
{
  "version": 1,
  "generation": 123,
  "device_name": "tsinghua.kundy",
  "operation": "activate"
}
```

`device_name` 是 CertServer 返回的部分永久名字，不包含 `.dhttp.net`；
helper 根据固定的 DHTTP 基础域拼出完整 Host、Origin、显示短名和 OAuth
callback。`generation` 由 Kundy 持久化：首次激活为 1，每次手动更新递增，旧版状态缺少该
字段时按 1 迁移；重启后可复用。helper 必须按 generation 幂等处理重复请求。
`operation` 为 `activate` 或 `deactivate`；旧版请求缺少该字段时按 `activate` 处理。
取消激活仍携带原设备名字和递增 generation，用于确保宿主机应用的是本次操作。同一次
取消激活的 generation 和意图必须在首个 helper 请求前持久化；CLI 重试、服务端控制器
和进程重启都复用该 generation，不能把尚未清理的身份重新解释为激活请求。

## 响应格式

```json
{
  "version": 1,
  "generation": 123,
  "state": "applied",
  "code": "applied",
  "upstream_host": "192-168-3-113.nip.io"
}
```

`state` 只有 `applied`、`already_applied` 或 `error`。失败时 `code` 使用
稳定的机器可读值，例如 `k3s_unavailable`、`configmap_invalid` 或
`pishoo_reload_failed`，不得返回私密配置。只有 runtime-config 成功响应包含
来自 `ConfigMap/flux-system/cluster-settings` 的非敏感 `upstream_host`；Kundy
校验其为 DNS Host 后，以普通运行用户权限原子生成目标 identity 的
`server.conf`。helper 必须先写临时文件、`fsync`
后再 rename 响应文件。
`stale_generation` 失败响应额外包含 `"current_generation": 123`，供旧版本升级后把
本地持久化水位推进到宿主机已应用代次以上；Kundy 仍以更大的新 generation 重试，
不会关闭 helper 的防回滚校验。

## 处理顺序

1. `runtime-config` helper 校验 `version`、`generation`、`device_name` 和
   `operation`。激活时先确保逐机 `oauth2-proxy-browser-dhttp` Cookie Secret 存在，再将固定的
   `kundy-device-identity` ConfigMap 更新为本次身份，并等待 k3s API 接受变更；
   随后用 Flux 标准 `reconcile.fluxcd.io/requestedAt` annotation 请求
   `authentik` 与 `kundy-dhttp` 两个固定 Kustomization 进行本次 reconcile。
   helper 拒绝 generation 回退和已绑定名字更换。ConfigMap 与 Secret 都不由
   Flux inventory 管理；Flux 只读取它们，
   通过 `postBuild.substituteFrom` 将值注入 OAuth、Authentik、Traefik 和
   SciFlow 模板。取消激活不会删除 Cookie Secret 或 CertServer 绑定，只把固定
   ConfigMap 改为 `DHTTP_ENABLED=false`、`DHTTP_REPLICAS=0` 并请求 Flux reconcile。
2. 激活时，`kundy` 收到 `applied` 和 `upstream_host` 后，以运行用户权限写入固定格式
   的 `<home>/.dhttp/<device_name>/server.conf`。配置反代
   `http://127.0.0.1:80`，将 `Host` 设为集群内部入口，并强制覆盖
   `X-Forwarded-Proto`、`X-Forwarded-Host`、`X-Forwarded-Port` 与
   `X-DHTTP-Origin`；然后写 `pishoo-reload.request`。
3. 取消激活时，Kundy 在 ConfigMap 禁用成功后先删除目标 identity 的 `server.conf`，
   再请求 `pishoo` helper。这样 Pishoo reload 时不会继续把待删除身份装入内存；证书和
   其余身份文件仍保留到 reload 成功，便于同 generation 重试。
4. `pishoo` helper 确认两个 Flux Kustomization 未暂停、持久化的
   `lastHandledReconcileAt` 已经处理当前 Kundy generation 且 `Ready=True`，再检查 identity、证书、`server.conf` 和
   Pishoo 全局配置，执行 reload 并确认服务 active，最后写
   `pishoo-reload.result`。取消激活时不要求本地 identity 存在，只检查禁用
   ConfigMap、等待 Flux，然后 reload Pishoo。等待逻辑不依赖可能被上层 GitOps
   清除的临时 `requestedAt` annotation；等待上限为 5 分钟，超时可由同 generation 重试。
5. 两步都成功后，`/api/runtime/status` 返回 `{"state":"ready"}`；任何
   一步尚未安装或没有响应都返回 `pending`，失败返回 `error`。证书身份本身
   不会因为 helper 暂时不可用而删除。Kundy 内部运行态控制器先只读检查结果，只有
   操作未完成时才用同一个 generation 继续；信息页和公开 API 不提供运行态写入口。

## 安全边界

- helper 只允许修改预先固定的 ConfigMap、Cookie Secret、两个 Flux reconcile
  annotation、namespace 和 Pishoo 服务，
  不接受来自请求的命令、路径、namespace 或 kubeconfig。
- helper 需要对 `device_name` 做和 CertServer/DHTTP 相同的 label 校验，
  防止路径穿越和任意 Host 注入。
- 所有正式 k3s 资源仍必须提交到 Flux Git 仓库；本协议不是绕过 Flux 的
  现场 `kubectl apply` 接口。
