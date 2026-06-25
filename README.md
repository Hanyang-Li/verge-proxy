# verge-proxy

Clash Verge 代理环境变量管理工具。读取 Clash Verge 的 `mixed-port`,在当前 shell 中设置/移除 `http_proxy`、`https_proxy`、`all_proxy`、`no_proxy` 环境变量。

## 安装

```sh
# 放到 PATH 下
sudo cp verge-proxy /usr/local/bin/verge-proxy
sudo chmod +x /usr/local/bin/verge-proxy

# 写入 zsh wrapper function 与补全(首次使用)
verge-proxy install
source ~/.zshrc
```

`--install` 写入的 wrapper 区块由一对标记包裹:

```
# >>> verge-proxy >>>
...
# <<< verge-proxy <<<
```

升级新版本后再次执行 `verge-proxy install` 会**整体替换标记之间的内容**(标记外的配置原样保留);若 `~/.zshrc` 中尚无该区块,则追加到文件末尾。因此可以反复执行,始终幂等。

## 用法

```sh
verge-proxy start      # 读取端口并开启代理
verge-proxy stop       # 移除代理环境变量
verge-proxy restart    # 重新读取端口并更新代理
verge-proxy node       # 实时测速并切换“手动选择”代理组节点
```

### 自动选择节点:`node`

`verge-proxy node` 会实时读取 Clash Verge 运行时配置中的 controller 信息,优先使用
`external-controller-unix` Unix socket,不写死 controller 端口。它会查找名称中包含“手动选择”的
Selector 代理组,对候选节点实时请求 `/delay`,并切换到延迟最低的可用节点。

未配置国家列表时,会从该代理组的所有节点中选择实时延迟最低的节点:

```sh
verge-proxy node
```

配置 `VERGE_AUTO_SELECT_COUNTRIES` 后,会按英文逗号分隔的顺序模糊匹配节点名。例如先测试日本,
日本没有可连通节点时再测试新加坡,都不可用时再从不匹配这些国家的其他节点兜底:

```sh
VERGE_AUTO_SELECT_COUNTRIES=日本,新加坡 verge-proxy node
```

`verge-proxy install` 不会把 `VERGE_AUTO_SELECT_COUNTRIES` 写入 `# >>> verge-proxy >>>` 管理区块。
如果希望长期生效,请在该管理区块外自行设置该环境变量。升级后可再次执行 `verge-proxy install` 更新补全。

默认每个节点的实时延迟测试超时为 5000ms。如需临时调短或调长,可设置 `VERGE_DELAY_TIMEOUT_MS`:

```sh
VERGE_DELAY_TIMEOUT_MS=2000 verge-proxy node
```

### 临时代理:`vp`

`vp <命令>` 会在一个子 shell 中临时开启代理并执行命令,命令结束后代理环境变量不会残留到当前终端:

```sh
vp curl https://example.com    # 仅本次请求走代理
vp ggl                         # 支持展开 alias
```

## 工作原理

`verge-proxy start|stop|restart` 本身只把 `export ...` / `unset ...` 语句打印到 stdout,由 `~/.zshrc` 中的 wrapper function 通过 `eval` 在当前 shell 内执行,从而真正改变当前 shell 的环境变量(子进程无法修改父 shell 环境)。
