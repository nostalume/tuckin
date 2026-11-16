# tuckin

[简体中文](README.zh-CN.md)

Keep one dotfiles repository for all your machines. Tuckin can deploy a file you own, change a few lines in a file an application maintains, and keep secrets encrypted in the repository.

[Install](#install) · [Your first file](#first) · [Paths](#repository) · [Tags](#templates) · [Patches](#patch) · [Secrets](#seals) · [Scripts](#scripts)

<a id="install"></a>

## Install

Release archives cover Windows x64, Linux x64, and Apple Silicon macOS. [Download v0.1.0](https://github.com/nostalume/tuckin/releases/tag/v0.1.0) · [Build from source](#build)

**Scoop on Windows**

With Git installed, add the Tuckin repository as a Scoop bucket and install from it.

```powershell
scoop bucket add tuckin https://github.com/nostalume/tuckin
scoop install tuckin/tuckin
tuckin --version
```

To upgrade, run `scoop update` and then `scoop update tuckin`. The Release workflow updates the [Scoop manifest](tuckin.json) after publishing a new version.

**Nix on Linux or macOS**

The tagged flake builds Tuckin from source.

```sh
nix --extra-experimental-features 'nix-command flakes' profile add github:nostalume/tuckin/v0.1.0
tuckin --version
```

The flake in this checkout also offers `#binary`. It installs the published GitHub archive with a pinned checksum and skips the Rust build. Run this from the Tuckin source checkout.

```sh
nix --extra-experimental-features 'nix-command flakes' profile add "path:$PWD#binary"
```

**Install script**

Run the command for your platform from the directory containing the script. Each script verifies the release archive and installs into `~/.local/bin` by default. [Released PowerShell script](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.ps1) · [Released shell script](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.sh)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 v0.1.0
```

The PowerShell script in this checkout adds its default directory to your user PATH. After the command above, reopen your terminal and run `tuckin --version`. If you used the script bundled with `v0.1.0`, add `%USERPROFILE%\.local\bin` to user **Path** in Windows' **Edit environment variables for your account** dialog first. To confirm the installation immediately, run `& "$HOME\.local\bin\tuckin.exe" --version`.

```sh
sh install.sh v0.1.0
export PATH="$HOME/.local/bin:$PATH"
tuckin --version
```

If the shell needs that `export` on each start, put it in the startup file it already uses, such as `~/.bashrc` for interactive non-login Bash or `~/.zshrc` for interactive Zsh. The shell script in this checkout prints the exact command for the chosen directory. It never edits startup files.

Set `TUCKIN_INSTALL_DIR` to choose another location, for example a directory already on PATH. The Windows installer leaves PATH to you when you choose a custom directory.

<a id="first"></a>

## Deploy your first file

Start in a new empty directory you can use as a demo repository. Pick an unused target name in your home directory for the exercise. Run `tuckin init`, then create `home/.tuckin-demo` in the repository with two lines.

```ini
theme=light
zoom=125
```

Run these from the repository.

```text
tuckin diff --patch home/.tuckin-demo
tuckin apply --no-scripts home/.tuckin-demo
tuckin diff
```

The first preview says `create` and exits with 1 to signal pending work. Open `~/.tuckin-demo` and you'll find the two lines you wrote. The final preview is empty with exit status 0. Tuckin leaves other files in your home directory alone.

For an existing repository, enter its directory and start with `tuckin diff`. To clone a trusted repository with Git, run `tuckin init --from <repository>` in an empty directory.

<a id="repository"></a>

## Give one file the right path on each machine

Suppose you keep Zed's settings at `home/@config/zed/settings.json`. Add these rows to `tuckin.toml` to map the same source to Zed's platform directory.

```toml
[map.home]
win = "dirs:home"
posix = "dirs:home"

[map."home/@config/zed"]
win = "dirs:config/Zed"
linux = "dirs:config/zed"
posix = ".config/zed"
```

On Windows, `dirs:config/Zed` starts at the OS configuration directory. On another POSIX system that uses the fallback, `.config/zed` starts at the parent mapping, here your home directory. Keep using the source path in commands, even when the destination differs on each machine.

<a id="templates"></a>

## Give two machines different roles

Say you want to keep the same repo on a workstation and a Linux server. On the server, put `headless` on a line by itself in `tuckin.tags`, alongside `tuckin.toml`. Add `/tuckin.tags` to Git's ignore file so each machine can choose its own roles. With the mapping above, add this rule to `tuckin.toml`.

```toml
[[ignore]]
platform = "linux"
tags = ["headless"]
paths = ["home/@config/zed/**"]
```

Run `tuckin status` on the server. Zed drops out of its plan. A Linux workstation without that tag still sees Zed in its plan. The same tag can choose a template value. For example, put `editor={{editor}}` in a managed file and add `editor = "vi"` under `[data]` and `editor = "nano"` under `[data."@headless"]`.

<a id="patch"></a>

## Manage one line in a changing file

Imagine an app owns the zoom value from the first exercise and you only want to control the theme. Replace the repository copy of `home/.tuckin-demo` with this single line. Keep the existing target from the exercise.

```handlebars
{{#patch "line" prefix="theme="}}dark{{/patch}}
```

Run `tuckin diff --patch home/.tuckin-demo` and apply the file again. You'll see `theme=dark` in the target beside the original `zoom=125`. The app can update zoom after that, and a future apply still owns only the theme's value. If a target already has a stable `zoom=` line but might lack a theme line, add `insert_before="zoom="` to the header. For a group of lines, use a [patch between two markers](docs/spec.md#patch).

<a id="seals"></a>

## Store a secret as ciphertext

Create another test file at `~/.tuckin-demo-token`, with a fake token such as `demo-value`. Run these commands from the demo repository.

```text
tuckin init --key
tuckin add --encrypt home/.tuckin-demo-token
tuckin diff --patch home/.tuckin-demo-token
```

The repository now has a `seal` block in place of the token. Tuckin creates or reuses a local private identity and can set its public key in `tuckin.toml`. Keep the private identity and its backup outside the repository. To manage a token within a larger file, give its patch a name and use `tuckin edit PATH --seal=NAME`. See [identity paths and recovery](docs/spec.md#seals) when moving to another machine.

<a id="scripts"></a>

## Run setup after deployment

For a trusted repository, place a setup script such as `scripts/gate-10-setup.ps1` in `scripts/`. That example uses PowerShell 7. `tuckin apply` writes files first and then runs setup. A successful `gate-` script can be reused on a later apply. Use `tuckin apply --no-scripts` whenever you only want file changes.

<a id="cli"></a>
<a id="deployment"></a>

## Commands and reference

Use `tuckin --help` to find commands and `tuckin apply --help` for an individual command. `status` lists planned actions, `diff` shows what needs to change, and `apply` deploys it. Use `add` to adopt an existing file, `edit` to open a source with the executable named by `EDITOR`, and `absorb` to collect supported target changes back into a source.

[Behavior specification](docs/spec.md) · [Architecture](docs/architecture.md)

<a id="build"></a>

## Build from source

Run `cargo build --release --locked` with Rust 1.88 or newer. Find the executable at `target/release/tuckin` or `target/release/tuckin.exe` on Windows.
