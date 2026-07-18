# verge-proxy

Clash Verge CLI。使用 Clash Verge/mihomo controller 读取运行状态、切换模式/代理组/节点，并在当前 zsh 中设置或移除代理环境变量。

## 安装

```sh
curl -fsSL https://raw.githubusercontent.com/Hanyang-Li/verge-proxy/main/install.sh | sh
```

脚本会从 GitHub Releases 下载 Apple Silicon（M 系列）二进制到 `/usr/local/bin`，并执行 `verge-proxy install`。也可以设置 `VERSION=v0.2.0` 安装指定版本。

手动安装：

```sh
cargo build --release
sudo cp target/release/verge-proxy /usr/local/bin/verge-proxy
verge-proxy install
source ~/.zshrc
```

`install` 会创建/更新 `~/.config/verge-proxy/config.yaml`，生成 `~/.config/verge-proxy/completions/_verge-proxy`，并软链接到 `$(brew --prefix)/share/zsh/site-functions/_verge-proxy`。它也会用标记区块更新 `~/.zshrc` 中的 wrapper：

```text
# >>> verge-proxy >>>
...
# <<< verge-proxy <<<
```

重复执行 `verge-proxy install` 会替换标记区块，标记外内容保持不变。

## 命令

```sh
verge-proxy start      # 设置当前 zsh 进程的代理环境变量，并显示 status
verge-proxy stop       # 移除当前 zsh 进程的代理环境变量
verge-proxy restart    # 重新读取端口并更新代理环境变量，并显示 status
verge-proxy status     # 用 prompt 显示 mode/group/node/delay/port，相邻 tag 紧挨排列，宽度不足时按段换行
verge-proxy mode       # 交互切换 规则 / 全局 / 直连
verge-proxy group      # 交互切换代理组，最多显示 10 行
verge-proxy node       # 交互切换节点，最多显示 10 行
verge-proxy port       # 只输出当前 mixed-port 数字
verge-proxy auto-node  # 并发测速并切换到最快节点
verge-proxy install    # 配置 ~/.config/verge-proxy、completion、~/.zshrc
```

`port` 和 `status` 的端口都动态读取：优先从 controller 的 `/configs["mixed-port"]` 获取，失败时回退到 Clash Verge 配置文件中的 `mixed-port`。

`node --filter <关键字>` 会先按关键字筛选节点，再进入 `dialoguer::Select` 列表选择；选择器内部不做筛选。

## 配置

配置文件位于 `~/.config/verge-proxy/config.yaml`：

```yaml
filter: "日本,新加坡"
concurrency: 20
timeout_ms: 2000
mode: "rule"
group: "🔰 手动选择"
prompt:
  mode_icon: "󰒓"
  group_icon: "󰓹"
  node_icon: "󰍍"
  delay_icon: "󱎫"
  port_icon: "󰤨"
```

`mode`/`group` 是 tag 显示的默认值：当前 mode 或 group 等于配置值时，status 类输出会隐藏对应 tag，直接显示后边的 tag。status 的 tags 采用紧凑布局：每行只有第一个 tag 显示左右圆角胶囊，其余 tag 紧挨左侧 tag、只显示右胶囊，放不下时按段换行。

当前 group/node 来自 Clash Verge controller 运行状态，不会写入 verge-proxy 配置文件。

`auto-node` 会按逗号分隔的范围逐个匹配节点名。上一个范围没有可连通节点时，才测试下一个范围；所有范围都失败后再测试其他节点。

临时覆盖范围：

```sh
verge-proxy auto-node --filter 日本,新加坡
```

`--filter` 的优先级高于配置文件。

## vp

`vp <命令>` 只给该命令所在的子 shell 设置代理，不影响当前 zsh 进程：

```sh
vp curl https://example.com
vp ggl
```

`start`、`stop`、`restart` 需要通过 `install` 写入的 zsh wrapper 执行，因为子进程不能直接修改父 shell 环境变量。

## 发布

推送以 `v` 开头的 tag 会触发 GitHub Action，构建 `aarch64-apple-darwin`（Apple Silicon）的 release 二进制并上传到对应 Release：

```sh
git tag v0.2.0
git push origin v0.2.0
```
