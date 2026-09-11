# D2VM-flux validation 运行手册

本文记录 Kundy 联调期间验证 `D2VM-flux` 修改的可靠流程。核心原则是：**先同步远端，
再在最新代码上运行完整 validation**。同步前后都要保护用户已有的工作区和 Git index。

## 1. 进入仓库并检查状态

当前联调仓库和分支为：

```bash
cd /Users/x/code/yuanwuzhi/D2VM-flux
git status --short --branch
git diff --cached --name-only
```

如果 `git diff --cached --name-only` 有输出，说明用户已有 staged 内容。不要执行
`git add`、`git reset` 或其他会改变 index 的操作，也不要直接开始 rebase；先确认 staged
内容的归属和处理方式。

如果只有需要保留的未暂存修改，可以使用 Git 自带的 autostash 同步当前联调分支：

```bash
git pull --rebase --autostash origin dev161
```

完成后检查：

```bash
git status --short --branch
git diff --cached --name-only
git log -1 --oneline --decorate
```

成功时本地 `dev161` 应与 `origin/dev161` 指向同一提交，autostash 应已自动恢复，
`git diff --cached --name-only` 仍无输出。如果 autostash 产生冲突，应停止 validation，
先解决冲突并重新检查工作区。

> validation 必须在 pull/rebase 之后运行。远端更新后，旧基线上的通过结果不再有效。

## 2. macOS 运行环境

仓库的 `scripts/validate-sciflow-release.sh` 使用 Bash 关联数组。macOS 自带的
`/bin/bash` 通常是 3.2，不能正确执行该脚本，典型错误为：

```text
./scripts/validate-sciflow-release.sh: line 59: IMAGE_AGENT: unbound variable
```

这不是 YAML 或本次修改失败，不要为绕过它而修改 validation 脚本。应使用 Bash 5，
并确保子脚本通过 `#!/usr/bin/env bash` 解析到同一版本。

仓库脚本声明的工具要求和本次验证通过的版本如下：

| 工具 | 脚本要求 | 本次验证版本 |
| --- | --- | --- |
| Bash | 支持关联数组，建议 5.x | 5.3.15 |
| yq | 4.48 | 4.48.1 |
| kustomize | 5.7 | 5.7.1 |
| kubeconform | 0.7 | 0.7.0 |

如果本机使用 Homebrew，可安装缺少的工具：

```bash
brew install bash yq kustomize kubeconform
```

确认版本：

```bash
D2VM_VALIDATION_BASH="$(brew --prefix bash)/bin/bash"
"$D2VM_VALIDATION_BASH" --version | sed -n '1p'
yq --version
kustomize version
kubeconform -v
```

若不希望安装到系统，也可以把兼容版本放入 `/tmp` 临时目录；关键是运行主脚本时将
Bash 5 和三个工具所在目录放在 `PATH` 最前面。

## 3. 运行完整 validation

直接观察全部输出：

```bash
D2VM_VALIDATION_BASH="$(brew --prefix bash)/bin/bash"
D2VM_VALIDATION_BIN="$(dirname "$D2VM_VALIDATION_BASH")"
PATH="$D2VM_VALIDATION_BIN:/opt/homebrew/bin:/usr/local/bin:$PATH" \
  "$D2VM_VALIDATION_BASH" ./scripts/validate.sh
```

脚本输出非常多，通常需要数分钟。更适合把输出保存到 `/tmp`，再查看进度和最终结果：

```bash
D2VM_VALIDATION_BASH="$(brew --prefix bash)/bin/bash"
D2VM_VALIDATION_BIN="$(dirname "$D2VM_VALIDATION_BASH")"
D2VM_VALIDATION_LOG="$(mktemp /tmp/d2vm-flux-validation.XXXXXX.log)"
printf 'validation log: %s\n' "$D2VM_VALIDATION_LOG"

PATH="$D2VM_VALIDATION_BIN:/opt/homebrew/bin:/usr/local/bin:$PATH" \
  "$D2VM_VALIDATION_BASH" ./scripts/validate.sh \
  >"$D2VM_VALIDATION_LOG" 2>&1
D2VM_VALIDATION_STATUS=$?

tail -80 "$D2VM_VALIDATION_LOG"
echo "validation exit code: $D2VM_VALIDATION_STATUS"
test "$D2VM_VALIDATION_STATUS" -eq 0
```

运行期间可在另一个终端把 `<实际日志路径>` 替换为 `D2VM_VALIDATION_LOG` 打印出的路径，
再查看进度：

```bash
tail -f <实际日志路径>
```

也可以只查看当前 validation 阶段：

```bash
rg 'INFO - Validating kustomization|INFO - Validating Grafana' \
  <实际日志路径> | tail
```

完整脚本依次执行：

1. SciFlow release bundle 一致性检查。
2. 下载 Flux OpenAPI schemas。
3. 全仓 YAML 语法解析。
4. 顶层集群资源 kubeconform 检查。
5. 所有 Kustomization 的 render 和 schema 检查。
6. Grafana 安全边界检查。

输出中的 `Secret skipped`、`CustomResourceDefinition skipped`、`Middleware skipped` 等是
脚本显式配置的预期行为，不代表失败。**最终退出码 0 才是完整 validation 通过的权威判据**；
正常结束前会看到：

```text
INFO - Validating Grafana security boundary
```

## 4. 本次 Kundy Job 修改的局部确认

`clusters/components/kundy-authentik/oauth2-blueprint-reconcile.yaml` 中的 Job 名应同时包含
runtime generation 和 enabled 状态，避免启停切换修改 Kubernetes 不可变的 Job Pod template：

```yaml
name: authentik-oauth2-blueprint-${DHTTP_CONFIG_GENERATION:=0}-${DHTTP_ENABLED:=false}
```

可在完整 validation 前做一个快速 substitution 检查：

```bash
sed -E \
  -e 's/\$\{DHTTP_CONFIG_GENERATION:=0\}/2/g' \
  -e 's/\$\{DHTTP_ENABLED:=false\}/true/g' \
  clusters/components/kundy-authentik/oauth2-blueprint-reconcile.yaml \
  | rg 'name: authentik-oauth2-blueprint'

sed -E \
  -e 's/\$\{DHTTP_CONFIG_GENERATION:=0\}/2/g' \
  -e 's/\$\{DHTTP_ENABLED:=false\}/false/g' \
  clusters/components/kundy-authentik/oauth2-blueprint-reconcile.yaml \
  | rg 'name: authentik-oauth2-blueprint'
```

预期得到两个不同名字：

```text
name: authentik-oauth2-blueprint-2-true
name: authentik-oauth2-blueprint-2-false
```

局部确认不能替代 `./scripts/validate.sh`。

## 5. validation 后检查

```bash
git status --short --branch
git diff --cached --name-only
git diff --check
git diff --stat
```

确认事项：

- 分支仍与预期远端分支同步。
- 只有预期的工作区修改。
- staged/index 状态没有变化。
- validation 没有在仓库中生成新文件。
- `git diff --check` 没有空白错误。

未经用户明确要求，不执行 `git add`、commit 或 push。

## 6. 常见问题

### `IMAGE_AGENT: unbound variable`

使用了 macOS Bash 3.2。改用 Bash 5，并把 Bash 5 目录放在 `PATH` 最前面，让子脚本也使用
同一 Bash；不要修改 release validation 逻辑来掩盖环境问题。

### `yq`、`kustomize` 或 `kubeconform` 找不到

安装脚本要求的兼容版本，或把临时二进制目录加入 `PATH`。运行前打印版本，避免误用旧工具。

### Flux schema 下载失败

`scripts/validate.sh` 会访问 GitHub 下载 schemas。检查网络、代理和 GitHub 可达性后重跑；
不要把下载失败报告为 manifest 校验失败。

### 输出很久没有结束

仓库包含大量集群 overlay，完整 render 会产生数万行输出并运行数分钟。检查日志是否仍在出现
新的 `INFO - Validating kustomization`。只要进程仍在推进且没有非零退出，就继续等待。

### pull/rebase 后文件发生变化

重新检查本地修改是否仍符合最新上游结构，并重新运行完整 validation。不要沿用 rebase 前的结果。
