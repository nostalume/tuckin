# tuckin

[English](README.md)

用一个仓库管理多台机器的 dotfiles。你可以部署整份配置，也可以只接管应用配置里的几行，并把敏感值加密后放进仓库。

[安装](#install) · [部署第一个文件](#first) · [跨平台路径](#repository) · [标签](#templates) · [补丁](#patch) · [密封](#seals) · [脚本](#scripts)

<a id="install"></a>

## 安装

发行包支持 Windows x64、Linux x64 和 Apple Silicon macOS。[下载 v0.1.2](https://github.com/nostalume/tuckin/releases/tag/v0.1.2) · [从源码构建](#build)

**Windows 上的 Scoop**

安装 Git 后，把 Tuckin 仓库添加为 Scoop bucket，再从中安装。

```powershell
scoop bucket add tuckin https://github.com/nostalume/tuckin
scoop install tuckin/tuckin
tuckin --version
```

升级时先运行 `scoop update`，再运行 `scoop update tuckin`。每次发布新版后，Release 工作流会更新仓库里的 [Scoop manifest](tuckin.json)。

**Linux 或 macOS 上的 Nix**

Nix 默认安装校验和固定的 GitHub 发行二进制。

```sh
nix --extra-experimental-features 'nix-command flakes' profile add github:nostalume/tuckin
tuckin --version
```

用 `#source` 从所选版本的源码构建。

```sh
nix --extra-experimental-features 'nix-command flakes' profile add 'github:nostalume/tuckin#source'
```

**安装脚本**

在脚本所在目录运行对应平台的命令。脚本会校验发行包，默认将程序安装到 `~/.local/bin`。[发行版 PowerShell 脚本](https://github.com/nostalume/tuckin/releases/download/v0.1.2/install.ps1) · [发行版 shell 脚本](https://github.com/nostalume/tuckin/releases/download/v0.1.2/install.sh)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 v0.1.2
```

PowerShell 脚本会把默认安装目录加入用户 PATH。重新打开终端后运行 `tuckin --version`。也可用 `& "$HOME\.local\bin\tuckin.exe" --version` 直接确认。

```sh
sh install.sh v0.1.2
export PATH="$HOME/.local/bin:$PATH"
tuckin --version
```

如果每次启动 shell 都需要这行 `export`，把它写入 shell 实际读取的启动文件，例如交互式非登录 Bash 的 `~/.bashrc` 或交互式 Zsh 的 `~/.zshrc`。当前源码中的 shell 脚本会为所选目录输出可直接执行的命令，启动文件由你管理。

可以通过 `TUCKIN_INSTALL_DIR` 选择其他安装目录，例如一个已在 PATH 里的目录。Windows 安装脚本在使用自选目录时由调用者管理 PATH。

<a id="first"></a>

## 部署第一个文件

找一个新建的空目录作为练习仓库，并为练习选择主目录里尚未使用的文件名。运行 `tuckin init`，再在仓库里新建 `home/.tuckin-demo`，写入两行。

```ini
theme=light
zoom=125
```

以下命令在仓库目录运行。

```text
tuckin diff --patch home/.tuckin-demo
tuckin apply --no-scripts home/.tuckin-demo
tuckin diff
```

第一次预览显示 `create`，退出码 1 表示还有待写项。打开 `~/.tuckin-demo`，可以看到刚写的两行。最后一次预览输出为空，退出码为 0。主目录里的其他文件留在原处。

已有仓库时，进入其目录后从 `tuckin diff` 开始。普通 `tuckin init` 在空目录创建骨架，无需 Git。要克隆信任的仓库，先确保 Git 可用，再在空目录运行 `tuckin init --from <repository>`。如果手头只有仓库归档，解包后在里面运行 `tuckin status`。

<a id="repository"></a>

## 让同一份配置落到各平台的正确位置

假设你把 Zed 的设置放在 `home/@config/zed/settings.json`。在 `tuckin.toml` 添加以下映射，让同一份源在各个平台找到 Zed 的配置目录。

```toml
[map.home]
win = "dirs:home"
posix = "dirs:home"

[map."home/@config/zed"]
win = "dirs:config/Zed"
linux = "dirs:config/zed"
posix = ".config/zed"
```

Windows 上的 `dirs:config/Zed` 从系统配置目录开始。使用后备值的 POSIX 系统上，`.config/zed` 从上一级映射开始，这里就是主目录。运行命令时仍使用同一个源路径。

<a id="templates"></a>

## 给两台机器分配不同角色

假设工作站和一台 Linux 服务器共用仓库。服务器上，在 `tuckin.toml` 旁建一个 `tuckin.tags`，单独一行写 `headless`。把 `/tuckin.tags` 加入 Git 忽略文件，让机器各自选择角色。使用上一节的映射，再向 `tuckin.toml` 添加这条规则。

```toml
[[ignore]]
platform = "linux"
tags = ["headless"]
paths = ["home/@config/zed/**"]
```

在服务器上运行 `tuckin status`，计划里就没有 Zed。未启用该标签的 Linux 工作站仍会管理它。标签也可以选择模板值。例如在源里写 `editor={{editor}}`，在 `[data]` 配置 `editor = "vi"`，再在 `[data."@headless"]` 配置 `editor = "nano"`。

<a id="patch"></a>

## 只管理应用配置里的一行

回到第一个练习。假设应用要自己保存缩放值，而你只想控制主题。保留主目录里的目标，把仓库的 `home/.tuckin-demo` 换成这一行。

```handlebars
{{#patch "line" prefix="theme="}}dark{{/patch}}
```

运行 `tuckin diff --patch home/.tuckin-demo`。从整文件改为补丁会引入一个新管理范围，用 `tuckin apply --no-scripts --overwrite home/.tuckin-demo` 确认这次更新。目标里会出现 `theme=dark`，旁边仍是原来的 `zoom=125`。以后应用修改缩放，再次部署也只管理主题值。如果目标已有稳定的 `zoom=` 行，而主题行可能还没出现，可以在补丁头部加 `insert_before="zoom="`。管理多行时可用[两个标记之间的补丁](docs/spec.zh-CN.md#patch)。

<a id="seals"></a>

## 把密文存入仓库

在 `~/.tuckin-demo-token` 创建另一个测试文件，写入 `demo-value` 这样的假令牌。以下命令在练习仓库运行。

```text
tuckin init --key
tuckin add --encrypt home/.tuckin-demo-token
tuckin diff --patch home/.tuckin-demo-token
```

仓库里现在用 `seal` 块保存密文。Tuckin 会创建或复用本地私钥，也可以把对应的公钥写进 `tuckin.toml`。把私钥和备份留在仓库外。要交互式修改密封文件，用 `EDITOR` 指定可信且会等待文件关闭的编辑器，再运行 `tuckin edit home/.tuckin-demo-token`。Windows 上需要 Tuckin 0.1.2 或更新版本，临时目录须位于本机 NTFS 卷。若令牌只是配置里的一个字段，可以给补丁命名，再用 `tuckin edit PATH --seal=NAME`。换机器时可查阅[身份路径与恢复](docs/spec.zh-CN.md#seals)。

<a id="scripts"></a>

## 部署后运行初始化

在信任的仓库中，可以把初始化脚本放到 `scripts/`，例如用 PowerShell 7 运行的 `scripts/gate-10-setup.ps1`。`tuckin apply` 先写文件，再运行初始化脚本。以 `gate-` 开头的脚本成功后可在后续部署中复用结果。只想部署文件时，运行 `tuckin apply --no-scripts`。

<a id="cli"></a>
<a id="deployment"></a>

## 命令与参考

如果 `apply` 提示会覆盖已有改动，先用 `tuckin diff --patch home/.gitconfig` 看差异。想保留本机改动，可以对支持回收的源运行 `tuckin absorb home/.gitconfig`。确定采用仓库版本时，运行 `tuckin apply --overwrite home/.gitconfig`。普通 `apply` 只列出少量已写入的路径。需要完整清单时加 `--list`，想看本次实际写入的单文件片段时加 `--patch home/.gitconfig`。

用 `tuckin --help` 找命令，用 `tuckin apply --help` 查具体用法。`status` 列出计划，`diff` 查看待改项，`apply` 执行部署。`add` 将已有文件收进仓库，`edit` 用 `EDITOR` 指定的程序打开源，`absorb` 将受支持的目标改动收回源。

[行为规范](docs/spec.zh-CN.md) · [架构](docs/architecture.zh-CN.md)

<a id="build"></a>

## 从源码构建

使用 Rust 1.88 或更新版本运行 `cargo build --release --locked`。可执行文件在 `target/release/tuckin`，Windows 上为 `target/release/tuckin.exe`。
