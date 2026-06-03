# verge-proxy

Clash Verge 代理环境变量管理工具。读取 Clash Verge 的 `mixed-port`,在当前 shell 中设置/移除 `http_proxy`、`https_proxy`、`all_proxy`、`no_proxy` 环境变量。

## 安装

```sh
# 放到 PATH 下
sudo cp verge-proxy /usr/local/bin/verge-proxy
sudo chmod +x /usr/local/bin/verge-proxy

# 写入 zsh wrapper function 与补全(首次使用)
verge-proxy --install
source ~/.zshrc
```

## 用法

```sh
verge-proxy start      # 读取端口并开启代理
verge-proxy stop       # 移除代理环境变量
verge-proxy restart    # 重新读取端口并更新代理
```

### 临时代理:`vp`

`vp <命令>` 会在一个子 shell 中临时开启代理并执行命令,命令结束后代理环境变量不会残留到当前终端:

```sh
vp curl https://example.com    # 仅本次请求走代理
vp ggl                         # 支持展开 alias
```

## 工作原理

`verge-proxy start|stop|restart` 本身只把 `export ...` / `unset ...` 语句打印到 stdout,由 `~/.zshrc` 中的 wrapper function 通过 `eval` 在当前 shell 内执行,从而真正改变当前 shell 的环境变量(子进程无法修改父 shell 环境)。
