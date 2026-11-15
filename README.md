# tuckin

[简体中文](README.zh-CN.md)

Tuckin deploys dotfiles from one repository across machines. Own an entire file, own a few lines of a file an application keeps updating, or store sensitive bytes in an age seal.

[Install](#install) · [First deployment](#first) · [Paths](#repository) · [Tags and templates](#templates) · [Patches](#patch) · [Secrets](#seals) · [Scripts](#scripts) · [Commands](#cli)

<a id="install"></a>

## Install

The `v0.1.0` archives cover Windows x64, Linux x64 and Apple Silicon macOS. You can also [build from source](#build). [Release downloads](https://github.com/nostalume/tuckin/releases/tag/v0.1.0)

**Scoop on Windows**

```powershell
scoop install https://github.com/nostalume/tuckin/releases/download/v0.1.0/tuckin.json
```

**Nix on Linux x64 or Apple Silicon macOS**

```sh
nix --extra-experimental-features 'nix-command flakes' profile add github:nostalume/tuckin/v0.1.0
```

**Install script**

Download the script for your platform, then run it from the download directory. The Windows script supports Windows PowerShell 5.1. [install.ps1](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.ps1) · [install.sh](https://github.com/nostalume/tuckin/releases/download/v0.1.0/install.sh)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 v0.1.0
```

```sh
sh install.sh v0.1.0
```

The scripts verify SHA-256 and install into `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows). Add that directory to PATH or set `TUCKIN_INSTALL_DIR` to a directory already on PATH. For a fork, set `TUCKIN_REPOSITORY` to its GitHub `OWNER/REPO`. Run `tuckin --version` to confirm `tuckin 0.1.0`.

<a id="first"></a>

## Deploy your first file

1. Run `tuckin init` in a new, empty directory.
2. Create a `home/` directory inside it. Add `home/.tuckin-example` with a line of text.
3. From the repository, preview, deploy, and preview again.

```text
tuckin diff --patch home/.tuckin-example
tuckin apply --no-scripts home/.tuckin-example
tuckin diff
```

The first preview returns 1 for a pending write. The file ends up at `~/.tuckin-example`. The last preview returns 0 with empty output when the files agree. `apply` leaves undeclared files in place.

For a trusted existing repository, run `tuckin init --from <repository>` in an empty directory. Git handles cloning and credentials. An existing checkout or extracted snapshot is usable directly.

<a id="repository"></a>

## Put each file in the right place

Commands that use an existing repository find the closest `tuckin.toml` in the current directory or an ancestor. `edit` can open it even while it contains invalid TOML. The supported top-level keys are `schema`, `map`, `ignore`, `data`, `identity_file` and `recipients`. All are optional. If set, `schema` is `1`. An unreadable or nonregular repository marker gives `E-G1`.

By default `home/` maps to the home directory, and `etc/` maps to `/etc` on POSIX or ProgramData on Windows. A custom `[map]` defines the managed roots. This example keeps one `home/@config/zed/settings.json` source even when its target directory changes.

```toml
[map.home]
win = "dirs:home"
posix = "dirs:home"

[map."home/@config/zed"]
win = "dirs:config/Zed"
linux = "dirs:config/zed"
posix = ".config/zed"
```

The longest prefix wins, ignoring case. `win` is Windows. `linux` and `macos` take precedence over the `posix` fallback. `dirs:config/Zed` starts at the OS configuration directory. `.config/zed` starts at the parent mapping, here the home directory. `dirs:` accepts `home`, `config`, `data`, `cache`, `programdata` and `documents`, with an optional suffix. `abs:` starts at an absolute path. Roots use `dirs:` or `abs:`, and multi-segment map keys start with `home/@anchor`. The unmatched suffix is appended literally.

Use repository paths like `home/.gitconfig` in commands. `/` is the canonical separator, and input also accepts `\` and a leading `./`. Selectors match whole path segments. Supply an exact file for single-file operations. Each admitted entry needs a mapping on its platform. Case and destination collisions are errors. A destination can be absent or a regular file. Parent directories may resolve through links, and final links, reparse points and other nonregular objects are refused. Discovery skips Git metadata, `tuckin.toml` and `scripts/` as deployment sources. Admitted source entries must be regular files reached without following source links.

<a id="templates"></a>

## Select machine roles with tags

Create a UTF-8 file named `tuckin.tags` beside `tuckin.toml`, with one active tag per line. Add `/tuckin.tags` to the repository's `.gitignore` to keep the selection local to each checkout. Tuckin treats the file as repository metadata. Blank lines and `#` comments are allowed. A missing file means no tags. If a tag repeats, its first position determines its order.

For example, put `work` and `headless` on separate lines. Then put these rules in `tuckin.toml`.

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

`apply` reads the tags automatically. The second ignore rule requires Linux **and both tags**. Any matching rule excludes the path. Globs match repository-relative paths. Match a directory's children to exclude its descendants. `posix` matches all non-Windows systems. Tags can stack and narrow deployment. Remove a tag to stop applying its layer and rules, then run `tuckin status` to see the new plan.

A UTF-8 source containing `{{` is interpreted as a template. With `editor={{editor}}` in a source, the default renders as `editor=vi`. The active OS layer (`@windows`, `@linux`, `@macos`) overrides defaults, then tags override in file order. The last value wins. Values are strings, integers, floats and booleans. A tag can be used in rules without a corresponding data layer.

Use `{{env "USER"}}` for a public environment value, `{{which "git"}}` for an executable path, `{{join "a" "b"}}` to join path segments, or `{{! comment }}` for a template comment. The helpers require defined inputs and `join` needs at least two arguments. The template vocabulary is limited to values, those helpers and comments. Output is raw text with one rendering pass. Write `\{{` for literal opening braces. Each run of two or more backslashes immediately before a tag loses one backslash and keeps the tag active.

<a id="patch"></a>

## Update only the bytes you own

A source containing a `patch` block updates an existing text file. For example, a target with an existing `"last_update":` line can keep its surrounding fields while Tuckin sets `"aria2-enabled":`.

```handlebars
{{#patch "line" prefix='"aria2-enabled":' insert_before='"last_update":'}} true,{{/patch}}
```

Locators are nonempty quoted strings. A unique line match keeps its indentation, prefix and line ending. The body owns the entire suffix on that physical line, including punctuation. If the line is absent, `insert_before` inserts it before a unique anchor line and uses the anchor's indentation and line ending. Keep the body inline and supply any required comma. Multiple insertions at one anchor follow declaration order. If the anchor has no ending, use the file's first ending or LF. Optional `after` and `before` together restrict a line search to a range of whole lines.

For several lines, preserve two markers and own the bytes between them.

```handlebars
{{#patch "between" after="# tuckin:begin\n" before="# tuckin:end"}}
export EDITOR={{editor}}
{{/patch}}
```

Use two distinct boundaries. `after` occurs once in the input and `before` once after it. Adjacent markers allow insertion. An empty body clears the interval or line suffix. Standalone tag lines disappear while the content newline before a closing block tag stays. Blocks share the original input. Their edits must be disjoint and a second application must require zero edits. Only text and comments may appear outside the patch blocks, where they are ignored. A body is a text template or a single seal, and a nonempty body rendering empty fails. Duplicate anchors, missing required matches, overlapping edits and unstable output produce `E-PA1`. A missing target or unusable layout needs initial text with stable anchors.

<a id="seals"></a>

## Keep secrets in the repository as ciphertext

Run `tuckin init --key` inside the repository. On first setup, Tuckin prepares the selected age X25519 identity and writes its public key to `recipients` in `tuckin.toml`. A file with several distinct keys needs an explicit recipient selection. When `recipients` is already configured, the command prints the selected identity file's public keys for use in an explicit policy change.

To try a whole-file seal, create a test file at `~/.tuckin-secret-example` and run the following in the repository. The repository copy holds ciphertext, and the preview omits secret contents.

```text
tuckin init --key
tuckin add --encrypt home/.tuckin-secret-example
tuckin diff --patch home/.tuckin-secret-example
```

The default identity path is `~/.config/age/keys.txt`. Set `identity_file` to an absolute path or one beginning with `~/` to choose another location. `AGE_IDENTITY_FILE` takes precedence. Tuckin creates the private key with owner-only permissions. Keep the identity and a recoverable backup outside the repository and deployment targets. `AGE_PASSPHRASE` unlocks an existing passphrase-protected identity.

- Adopt a whole secret file with `tuckin add --encrypt home/.ssh/id_ed25519`.
- Replace a whole sealed source with `tuckin edit home/.ssh/id_ed25519 --seal`. It reads hidden input or raw stdin.
- To seal a patch, add a unique name such as `name="token"` to the patch header and give it a literal body or `{{#seal}}{{/seal}}`. Run `tuckin edit home/app.conf --seal=token` to supply new bytes, or `tuckin absorb home/app.conf --seal=token` to collect the bytes from a trusted existing target. Bare `--seal` also selects a file's sole patch.

A seal stores single-line Base64 age ciphertext. It owns the whole file or the whole patch body. Names use ASCII letters, digits, underscores and hyphens and are case-sensitive. Locators and prefixes are public. The recipient list must contain at least one valid public key when provided. Duplicate keys collapse. Missing recipients prevent new ciphertext, and existing seals can still be opened with a matching identity. Missing identities, wrong passphrases, unmatched keys, empty seals and damaged ciphertext report distinct `E-E1` reasons. Equal contents keep their ciphertext unless fresh encryption is requested. Explicit sealing takes one exact source path, requires nonempty input and reports `sealed` or `unchanged`. A successful rekey reports `rekeyed`.

To rotate keys, point `AGE_IDENTITY_FILE` at a new path and run `tuckin init --key`. Set the new recipients and point back to the old identity while running `tuckin edit home/app.conf --rekey` for each affected source. Keep both identities until you verify the result using the new one. If the old key is lost, `tuckin absorb home/app.conf --rekey` can recover from a trusted deployed copy. Use `--seal=token --rekey` to force fresh encryption for one selected body with input bytes or the target. Old commits remain readable by their original recipients. Without a matching identity, deployed copy or original bytes, the ciphertext cannot be recovered.

Sealed targets give read and write access to the owner. POSIX uses mode `0600`. Windows uses a protected DACL on local NTFS. When Tuckin identifies unsupported storage, planning returns `E-P1` before writing files. POSIX supports protected sessions for external editors. On Windows, use `edit --seal` or `absorb --seal` for secret input. `diff` hides both the contents and byte counts of sealed targets.

<a id="scripts"></a>

## Automate machine setup

Place scripts such as `scripts/gate-10-setup.sh` in a flat `scripts/` directory. They may use public templates and obey the same ignore rules. Use `apply --no-scripts` for a file-only run.

| Extension | Runtime |
| --- | --- |
| `.sh` / `.bash` | `sh` / `bash` |
| `.lua` | Standalone `lua` |
| `.ps1` | PowerShell 7 (`pwsh`) |
| `.py` | Python 3 from the active virtual environment or executable search path, with the Windows launcher as an additional lookup |
| Other | OS execution on POSIX. Windows accepts `.exe`, `.cmd` and `.bat`. |

The listed script runtimes read the file directly. The extension selects the runtime even if the file has a shebang. Install the required runtime in advance. On Windows, use shells that can read native paths. The PowerShell child uses `-NoProfile` and on Windows also `-ExecutionPolicy Bypass`.

Tuckin resolves the required interpreters before file writes and runs scripts afterward in filename byte order, in the repository root and with your permissions and environment. Path selectors narrow file writes. Scripts still run unless explicitly skipped. A missing interpreter produces `E-P1` before publication. An execution failure stops later scripts and keeps earlier effects. Use `gate-` to reuse a successful setup at the same repository position, Git state, script name, dispatch and rendered bytes. Success is cached after the entire script phase succeeds. `TUCKIN_CACHE_DIR` can override the system cache location with a directory outside the repository.

<a id="cli"></a>
<a id="deployment"></a>

## Preview, deploy and collect changes

| Command | What it does |
| --- | --- |
| `init [--from REPOSITORY]` | Make a skeleton or clone into an empty directory. |
| `init --key` | Prepare the local identity and set the first recipient in an existing repository. |
| `status [PATH…]` | List `create`, `overwrite` and `unchanged`. `?` rows can identify unmanaged neighboring files. |
| `diff [PATH…]` | Summarize pending writes. Redirect stdout for the full TSV list. `--patch PATH` shows one bounded text preview. |
| `apply [PATH…]` | Publish planned files and run scripts. `--no-scripts` skips the script phase. |
| `add [--encrypt] PATH…` | Adopt existing targets under exact paths. Existing sources are refused. |
| `edit [PATH…]` | Open sources with the program named by `$EDITOR`. The default is `tuckin.toml`. See `--seal[=NAME]` and `--rekey` above. |
| `absorb [PATH…]` | Collect whole files and supported owned patch bytes from the targets. See `--seal[=NAME]` and `--rekey` above. |

Set `EDITOR` to an executable name or path before using `edit`. For example, in PowerShell set `$env:EDITOR = (Get-Command notepad.exe -CommandType Application).Source` and inspect `$env:EDITOR` to confirm the path. Use a launcher script for an editor that needs extra arguments.

`status`, `diff`, `apply` and `absorb` select the repository when paths are omitted. Planning validates placements before evaluating selected files. A failed plan writes nothing. Whole files can create a missing target. Existing contents are compared by byte, and private targets can be scheduled for protection repair even when bytes agree. `apply` creates parents as needed and publishes complete files one at a time. A later write failure preserves earlier writes and reports how many succeeded. Success prints `created`, `overwritten`, `unchanged` and `bytes` as TSV. An ordinary `absorb` skips missing targets and empty seals, preserves locators and unchanged ciphertext, and refuses generated patch bodies it cannot collect back. A missing target with a filled seal reports `unresealed`.

`diff` shows up to six paths in its terminal summary. A text preview is limited to 24 lines and 76 columns, with a calculation bound of 64 KiB and 4096 lines per side and 65,536 comparison units after trimming. Binary and sealed previews omit contents. Success returns 0, pending writes return 1 from `diff`, and failures return 2. Results go to stdout and diagnostics to stderr.

| Diagnostic | What to inspect |
| --- | --- |
| `E-G1` | Repository marker, TOML or `tuckin.tags` |
| `E-A1` / `E-A4` | Path spelling, platform mapping or missing anchor |
| `E-I1` / `E-T1` | Colliding destinations or nonregular source/target |
| `E-R1` / `E-PA1` | Template inputs or patch range and convergence |
| `E-AB1` | Target edits cannot be represented in the source body |
| `E-E1` / `E-P1` | Recipients, identity or ciphertext, or a required platform capability |

<a id="build"></a>

## Build from source

With Rust 1.88 or newer, run `cargo build --release --locked` in the source directory. The executable is `target/release/tuckin` or `target/release/tuckin.exe` on Windows. Cargo resolves Pulith from crates.io using the lockfile.

See [Architecture](docs/architecture.md) for the design and division of responsibilities.
