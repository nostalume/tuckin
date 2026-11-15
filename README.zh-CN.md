# tuckin

[English](README.md)

Tuckin 用一份仓库管理不同机器的 dotfiles。你可以接管整个文件，只更新应用配置中归你管理的几行，也可以用 age 封条保存敏感内容。

[安装](#install) · [首次部署](#first) · [路径映射](#repository) · [标签与模板](#templates) · [补丁](#patch) · [加密](#seals) · [脚本](#scripts) · [命令](#cli)

<a id="install"></a>

## 安装

`v0.1.0` 提供 Windows x64、Linux x64 和 Apple Silicon macOS 二进制包，也可[从源码构建](#build)。[发行下载](https://github.com/nostalume/tuckin/releases/tag/v0.1.0)

**Windows 上用 Scoop**

```powershell
scoop install https://github.com/nostalume/tuckin/releases/download/v0.1.0/tuckin.json
```

**Linux x64 或 Apple Silicon macOS 上用 Nix**

```sh
nix --extra-experimental-features 'nix-command flakes' profile add github:nostalume/tuckin/v0.1.0
```

**用安装脚本**

下载对应平台的脚本，在下载目录运行。Windows 脚本支持 Windows PowerShell 5.1。[install.ps1](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.ps1) · [install.sh](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.sh)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 v0.1.0
```

```sh
sh install.sh v0.1.0
```

脚本校验发行包的 SHA-256，并安装到 `~/.local/bin`，Windows 上是 `%USERPROFILE%\.local\bin`。将该目录加入 PATH，或用 `TUCKIN_INSTALL_DIR` 指定已有的 PATH 目录。使用 Fork 时，可将 `TUCKIN_REPOSITORY` 设为对应的 GitHub `OWNER/REPO`。运行 `tuckin --version` 应显示 `tuckin 0.1.0`。

<a id="first"></a>

## 部署第一个文件

1. 在新建的空目录中运行 `tuckin init`。
2. 在仓库里新建 `home/` 目录，再创建 `home/.tuckin-example` 并写入一行文字。
3. 在仓库中依次预览、部署、再次预览。

```text
tuckin diff --patch home/.tuckin-example
tuckin apply --no-scripts home/.tuckin-example
tuckin diff
```

第一次预览返回 1，表示有待写入内容。部署后，文件在 `~/.tuckin-example`。最后一次预览若无差异，返回 0 且无输出。`apply` 会保留未声明的本机文件。

已有可信仓库时，可以在空目录运行 `tuckin init --from <仓库>`。克隆和认证由 Git 处理。已有工作树或解包后的快照可直接使用。

<a id="repository"></a>

## 把同一份文件放到各平台的正确位置

使用现有仓库的命令从当前目录向上寻找最近的 `tuckin.toml`。即使 TOML 写坏了，`edit` 也能打开它。配置可用的顶层键是 `schema`、`map`、`ignore`、`data`、`identity_file` 和 `recipients`，均可省略。填写 `schema` 时使用 `1`。仓库标记无法读取或不是普通文件时报 `E-G1`。

默认 `home/` 指向用户主目录，`etc/` 在 POSIX 指向 `/etc`、在 Windows 指向 ProgramData。自定义 `[map]` 时，由它定义受管的根。下面让同一份 `home/@config/zed/settings.json` 在各平台落到对应的目录。

```toml
[map.home]
win = "dirs:home"
posix = "dirs:home"

[map."home/@config/zed"]
win = "dirs:config/Zed"
linux = "dirs:config/zed"
posix = ".config/zed"
```

最长匹配前缀优先，匹配时忽略大小写。`win` 对应 Windows，`linux`、`macos` 优先于 `posix` 回退。`dirs:config/Zed` 从系统配置目录开始，`.config/zed` 从父映射开始，这里是用户主目录。`dirs:` 可用 `home`、`config`、`data`、`cache`、`programdata`、`documents`，后面可加路径。`abs:` 后接绝对路径。根映射使用 `dirs:` 或 `abs:`，多段映射键以 `home/@锚点` 开头。未参与匹配的后续路径原样拼接。

命令中写 `home/.gitconfig` 这样的仓库路径。规范分隔符为 `/`，输入也接受 `\` 和开头的 `./`。选择器按完整路径段匹配，单文件操作使用准确路径。每个参与部署的条目都要有当前平台的映射。大小写冲突和目标重叠会报错。目标可以不存在或是普通文件，父目录可以经由链接解析，目标末端的链接、重解析点和其他非普通文件会被拒绝。Git 元数据、`tuckin.toml` 和 `scripts/` 位于部署源之外。扫描受管源时不跟随仓库内的链接，纳入部署的条目必须是普通文件。

<a id="templates"></a>

## 用标签叠加机器用途

在 `tuckin.toml` 旁创建 UTF-8 文本文件 `tuckin.tags`，每行写一个要启用的标签。在仓库的 `.gitignore` 加入 `/tuckin.tags`，让每份工作树保留自己的选择。Tuckin 将它作为仓库元数据读取。允许空行和 `#` 注释。文件不存在时标签为空，重复标签取第一次出现的位置。

例如，把 `work` 和 `headless` 分两行写入本机标签文件，再往 `tuckin.toml` 加入以下规则。

```toml
[[ignore]]
paths = ["README.md", "notes/**"]

[[ignore]]
platform = "linux"
tags = ["work", "headless"]
paths = ["home/@config/zed/**", "scripts/*-desktop.sh"]

[data]
editor = "vi"

[data."@windows"]
editor = "notepad"

[data."@work"]
editor = "nvim"
```

`apply` 自动读取标签。第二条规则要求 **Linux 且同时具有两个标签**。命中任意一条规则即可排除路径。通配符匹配仓库相对路径，要排除普通目录里的文件就匹配它的子路径。`posix` 匹配所有非 Windows 系统。标签可以叠加并缩小部署范围。移除标签可停用它的模板层和规则，再用 `tuckin status` 查看新的部署计划。

有效的 UTF-8 源文件含有 `{{` 时按模板处理。源文件里的 `editor={{editor}}` 默认渲染为 `editor=vi`。当前系统层（`@windows`、`@linux`、`@macos`）覆盖默认值，随后依标签文件的顺序覆盖，同一个键取最后的值。值支持字符串、整数、浮点数和布尔值。标签可以只用于忽略规则。

`{{env "USER"}}` 取公开环境变量，`{{which "git"}}` 取可执行文件路径，`{{join "a" "b"}}` 拼接路径，`{{! 注释 }}` 写模板注释。引用的输入必须存在，`join` 至少取两个参数。模板只支持值、上面的函数和注释。输出保留原文本，只渲染一次。用 `\{{` 写字面开括号。标签前有两个或更多连续反斜杠时消去一个，标签仍参与渲染。

<a id="patch"></a>

## 只改自己管理的字节

含有 `patch` 块的源文件会更新现有文本目标。例如目标已有 `"last_update":` 一行，这条补丁可以设置 `"aria2-enabled":` 并保留周围字段。

```handlebars
{{#patch "line" prefix='"aria2-enabled":' insert_before='"last_update":'}} true,{{/patch}}
```

定位器使用非空的引号字符串。前缀唯一匹配时，保留原来的缩进、前缀和行尾，块体接管同一物理行余下的内容和标点。缺行时，`insert_before` 在唯一锚点前插一行，沿用该行的缩进和换行。将块体写在同一行，按目标布局写逗号。同一锚点的插入按声明顺序排列。锚点没有行尾时用文件已有的第一种换行，否则用 LF。也可以同时给出 `after` 和 `before`，将搜索限制在其中的完整行。

涉及多行时，保留两个标记，接管中间的字节。

```handlebars
{{#patch "between" after="# tuckin:begin\n" before="# tuckin:end"}}
export EDITOR={{editor}}
{{/patch}}
```

两个边界必须不同。`after` 在输入中唯一，`before` 在它后面唯一。边界相邻时可插入，空块体可清除区间或行后缀。单独成行的标签会消去，独行结束标签前的内容换行会保留。各块在同一份原始输入定位，写入范围不得重叠，再执行一次应无修改。补丁块以外只允许文本和注释，其内容不参与更新。块体可以是文本模板或一个封条，声明了非空内容却渲染为空会失败。重复锚点、缺少必需匹配、重叠或不收敛会报 `E-PA1`。空目标需要先有带稳定锚点的初始文本。

<a id="seals"></a>

## 用密文保存秘密

在仓库内运行 `tuckin init --key`。首次设置时，Tuckin 准备所选路径的 age X25519 身份，并把公钥写入 `tuckin.toml` 的 `recipients`。身份文件含多把不同的私钥时，请显式选择收件人。已有 `recipients` 时，命令输出所选身份文件的公钥，供显式调整收件人使用。

要试用整文件封条，可先创建本机测试文件 `~/.tuckin-secret-example`，再在仓库中运行下列命令。仓库副本保存密文，预览会省略秘密内容。

```text
tuckin init --key
tuckin add --encrypt home/.tuckin-secret-example
tuckin diff --patch home/.tuckin-secret-example
```

私钥路径默认是 `~/.config/age/keys.txt`。`identity_file` 可指定绝对路径或以 `~/` 开头的路径，`AGE_IDENTITY_FILE` 优先于配置。生成的私钥以所有者专用权限写入，私钥和可恢复的备份放在仓库及部署目标之外。`AGE_PASSPHRASE` 用于解锁已有的口令保护身份。

- 收纳整份秘密文件时，运行 `tuckin add --encrypt home/.ssh/id_ed25519`。
- 填写或替换整份封条时，运行 `tuckin edit home/.ssh/id_ed25519 --seal`，从无回显输入或原始标准输入读取。
- 加密补丁字段时，在头部写一个唯一名字，例如 `name="token"`，块体使用字面文本或 `{{#seal}}{{/seal}}`。运行 `tuckin edit home/app.conf --seal=token` 输入新值，或用 `tuckin absorb home/app.conf --seal=token` 从可信的现有目标收纳。文件只有一个补丁时也可只写 `--seal`。

封条用一行 Base64 保存 age 密文，覆盖整文件或整个补丁块体。补丁名接受 ASCII 字母、数字、下划线和连字符，区分大小写。边界和前缀是公开文本。填写收件人列表时至少要有一个有效公钥，重复公钥只使用一次。生成新密文需要配置收件人，打开旧封条需要匹配的私钥。身份不可用、口令错误、密钥不匹配、空封条和密文损坏会以 `E-E1` 报告各自原因。内容相同时沿用原密文，除非显式要求重新加密。显式封条操作需要一个准确的源路径和非空输入，报告 `sealed` 或 `unchanged`。轮换成功报告 `rekeyed`。

轮换时先将 `AGE_IDENTITY_FILE` 指向新路径，运行 `tuckin init --key`。配置新公钥，再把本机身份路径指回旧私钥，对受影响的源逐个执行 `tuckin edit home/app.conf --rekey`。保留新旧私钥，直到验证新私钥能打开新的封条。若旧私钥丢失，可用 `tuckin absorb home/app.conf --rekey` 从可信的已部署副本恢复。用 `--seal=token --rekey` 可对单个补丁的输入或目标值强制重新加密。旧 Git 提交里的密文仍可由原收件人读取。没有匹配的私钥、已部署副本或原始明文时，无法恢复密文。

密文部署目标的读写权限授予文件所有者。POSIX 使用 `0600`，Windows 在本地 NTFS 上使用受保护的 DACL。确定存储不支持私有写入时，计划在写入任何文件前返回 `E-P1`。POSIX 支持受保护的外部编辑会话。Windows 上可用 `edit --seal` 或 `absorb --seal` 录入秘密。`diff` 会隐藏密文目标的内容和字节数。

<a id="scripts"></a>

## 用脚本准备机器

把 `scripts/gate-10-setup.sh` 这样的脚本平铺放在 `scripts/` 中。脚本可使用公开模板，并遵循相同的忽略规则。只部署文件时用 `apply --no-scripts`。

| 扩展名 | 解释器或启动方式 |
| --- | --- |
| `.sh` / `.bash` | `sh` / `bash` |
| `.lua` | 独立的 `lua` |
| `.ps1` | PowerShell 7（`pwsh`） |
| `.py` | 先找活跃的虚拟环境和 PATH 上的 Python 3，Windows 还可从 Python 启动器查询 |
| 其他 | POSIX 交给系统执行。Windows 接受 `.exe`、`.cmd` 和 `.bat`。 |

表中列出的解释器直接读取脚本。扩展名决定解释器，即使首行有 shebang。事先安装所需解释器。Windows 的 shell 应能读取原生 Windows 路径。PowerShell 子进程使用 `-NoProfile`，Windows 上也传入 `-ExecutionPolicy Bypass`。

Tuckin 在写文件前寻找所需解释器，写完后在仓库根目录按文件名的字节顺序执行脚本。脚本继承调用者的权限和环境。命令中的路径选择器缩小文件部署范围，脚本仍会运行。缺少解释器时在写文件前报 `E-P1`。脚本失败会中止后续脚本，保留已完成的操作。用 `gate-` 前缀可复用一次成功，条件是仓库位置、Git 状态、脚本名、派发方式和渲染后字节相同。整批脚本成功后写缓存。`TUCKIN_CACHE_DIR` 可指定一个仓库外的缓存目录。

<a id="cli"></a>
<a id="deployment"></a>

## 预览、部署和收回改动

| 命令 | 用途 |
| --- | --- |
| `init [--from 仓库]` | 在空目录新建或克隆仓库。 |
| `init --key` | 在现有仓库中准备本机私钥并设置首个收件人。 |
| `status [路径…]` | 列出 `create`、`overwrite`、`unchanged`，可能用 `?` 标记附近未纳管文件。 |
| `diff [路径…]` | 概览待部署文件。重定向取得完整 TSV 列表。用 `--patch 路径` 预览一个文件。 |
| `apply [路径…]` | 按计划写文件并执行脚本。用 `--no-scripts` 跳过脚本阶段。 |
| `add [--encrypt] 路径…` | 用准确路径收纳已有目标。仓库源已存在会拒绝。 |
| `edit [路径…]` | 用 `$EDITOR` 指定的程序编辑仓库源，默认打开 `tuckin.toml`。支持上文的 `--seal[=名字]` 和 `--rekey`。 |
| `absorb [路径…]` | 从目标收回整文件和可表示的补丁内容。支持上文的 `--seal[=名字]` 和 `--rekey`。 |

运行 `edit` 前，将 `EDITOR` 设为可执行程序名或路径。例如在 PowerShell 中用 `$env:EDITOR = (Get-Command notepad.exe -CommandType Application).Source`，随后输入 `$env:EDITOR` 确认路径。需要给编辑器传入额外参数时，用启动脚本包装。

`status`、`diff`、`apply` 和 `absorb` 省略路径时选择全仓库。计划先验证目标位置，再计算选中的文件。计划失败时目标不变。整文件可以创建不存在的目标。已有文件按字节比较，目标的私有权限有误时，即使内容相同也会安排修复。`apply` 按需创建父目录，逐个发布完整文件，后续写失败时保留已发布文件并报告数量。成功时以 TSV 输出 `created`、`overwritten`、`unchanged` 和 `bytes`。普通 `absorb` 跳过缺失目标和空封条，保留定位器与未变的密文，并拒绝无法收回的模板块体。已填封条的目标不存在时会报告 `unresealed`。

终端里的 `diff` 最多显示六条路径。单文件预览最多 24 行，每行 76 列，计算限制为每侧 64 KiB、4096 行和去除公共首尾后的 65536 个单元。二进制与密文省略正文。成功返回 0，`diff` 有待写入内容返回 1，出错返回 2。结果在标准输出，诊断在标准错误。

| 诊断 | 查哪里 |
| --- | --- |
| `E-G1` | 仓库标记、TOML 或 `tuckin.tags` |
| `E-A1` / `E-A4` | 路径拼写、平台映射或未解析的锚点 |
| `E-I1` / `E-T1` | 目标冲突、源或目标位置不是普通文件 |
| `E-R1` / `E-PA1` | 模板输入、补丁定位或收敛 |
| `E-AB1` | 目标改动无法表示为可写回的源块体 |
| `E-E1` / `E-P1` | 收件人、私钥、密文，或所需的平台能力 |

<a id="build"></a>

## 从源码构建

使用 Rust 1.88 或更新版本，在源码目录运行 `cargo build --release --locked`。程序在 `target/release/tuckin`，Windows 上是 `target/release/tuckin.exe`。Cargo 根据锁文件从 crates.io 获取 Pulith。

设计依据与责任划分见[架构](docs/architecture.zh-CN.md)。
