# tuckin behavior specification

[简体中文](spec.zh-CN.md) · [Tutorial](../README.md) · [Architecture](architecture.md)

This document defines the repository format, command effects, and observable failures. Examples in the tutorial use these rules.

<a id="repository"></a>

## Repository and placement

Commands working on an existing repository search from the current directory up to the closest `tuckin.toml`. A missing marker continues the search. An unreadable marker, a nonregular marker, or exhaustion of the search produces `E-G1`. Configuration is UTF-8 TOML. Its optional top-level keys are `schema`, `map`, `ignore`, `data`, `identity_file`, and `recipients`. If present, `schema` is the integer `1`. Unknown fields in the configuration, map rows, and ignore rules fail with `E-G1`. Ordinary `edit tuckin.toml` only needs the regular marker and can open a configuration with invalid contents.

| Declaration | Rule |
| --- | --- |
| Source spelling | A repository-relative UTF-8 path with at least two segments. Empty, `.`, and `..` segments, absolute paths, and leading `~` are invalid. A segment starting with `@` may appear once, immediately below `home`, with at least one segment after it. |
| Map key | A root such as `home`, or a prefix with at least three segments beginning `home/@anchor/`. Match whole segments case-insensitively. The longest matching key governs the entry. |
| Platform row | Windows selects `win`. Linux and macOS select `linux` and `macos` first, then `posix` in the same row. Other non-Windows systems select `posix`. A missing platform value on the selected row fails with `E-A1`. |
| `dirs:NAME[/suffix]` | Resolve the platform directory and append the optional literal suffix. Names are `home`, `config`, `data`, `cache`, `programdata`, and `documents`. An unavailable requested directory produces `E-G1`. |
| `abs:PATH` | Start at the given absolute path. Shell and environment substitutions are absent. |
| Relative value | Resolve against the longest proper prefix key for the same platform. Root keys require `dirs:` or `abs:`. Relative values and `dirs:` suffixes require nonempty segments with neither `.` nor `..`. |

Append the unmatched source suffix literally. An anchored source requires a matching anchor key. If `map` is absent or empty, `home` maps to the home directory and `etc` maps to ProgramData on Windows or `/etc` on POSIX. A nonempty map supplies the full mapping table. The platform must be able to represent the translated path. Segments are limited to 255 UTF-16 code units on Windows and 255 bytes on POSIX. Windows additionally rejects its reserved characters, device names, and segments ending in period or space (`E-P1`).

Root names beginning with `.git`, as well as `tuckin.toml`, `tuckin.tags`, and `scripts`, are skipped by file discovery. Windows compares these names case-insensitively. Source directory links are never traversed. Entries eligible for deployment must be regular files. Case collisions and symbolic target collisions are rejected for the layout, using per-platform exclusions with no active tags for the portable target comparison. The current machine also resolves existing destination parents, including directory aliases. Equal resolved positions, an occupied position serving as the parent of another entry, positions inside the repository, and positions reserved for the identity are refused. A final existing target must be a regular file. A link or other nonregular object at that position produces `E-T1`.

CLI selectors use repository spellings. Both separators are accepted, leading `./` is removed, and `..` is rejected. A prefix selector matches whole path segments case-sensitively. Every supplied selector must match, and the union retains repository spelling order. Exact-file operations require a file spelling. Selection limits contents evaluated, with placement still validated across all admitted entries on the machine.

<a id="templates"></a>

## Admission and template data

`tuckin.tags` beside the marker is UTF-8. An initial BOM is skipped. Each line ends at its first `#`, then surrounding whitespace is removed. Empty lines contribute nothing. A missing file supplies no active tags. An unreadable or nonregular tags file produces `E-G1`.

Each `[[ignore]]` rule supplies `paths` and may supply one `platform` and a list of `tags`. A rule applies when its platform matches and all listed tags are active. Omitted conditions impose no restriction. `posix` matches any non-Windows platform. A path is excluded if any applicable glob matches its repository spelling, including in the script phase. A directory match applies to that spelling. Exclude descendants explicitly with a pattern such as `notes/**`.

`[data]` contains scalar strings, integers, floats, and booleans, with flat override tables named `@NAME`. The evaluation order is defaults, the current OS layer (`@windows`, `@linux`, `@macos`, or the current OS name), then active tag layers in file order. A layer applies once at its first position, and later layers replace earlier values by key. Missing layers contribute nothing. Arrays, dates, nested layer tables, and invalid layer shapes produce `E-G1`.

A non-UTF-8 source, or a UTF-8 source without `{{`, is copied byte for byte. For a parsed text source Tuckin accepts scalar references, comments, and the following helpers. Missing input, unknown or unsupported syntax, and helper failures produce `E-R1`. Output is unescaped text from one rendering pass.

| Syntax | Meaning |
| --- | --- |
| `{{name}}` | Read a scalar from the effective data. |
| `{{env "NAME"}}` | Read a defined UTF-8 environment value. Use public values in templates. |
| `{{which "program"}}` | Resolve a program from the executable search path. |
| `{{join "a" "b"}}` | Join at least two arguments with the platform's path separator. |
| `{{! comment }}` | Emit nothing. |

Helper arguments are bare data keys or string, integer, and boolean literals. A key can refer to any supported scalar. A single backslash immediately before `{{` makes the opening braces literal. A run of two or more loses one backslash and leaves the tag active. Output introduced by a value or a helper is used as bytes. The same template rules apply to admitted scripts.

<a id="patch"></a>

## Byte patches

A source with `patch` blocks patches one existing target. The target is searched as bytes. Outside the blocks only text and comments are allowed, and they contribute no output. A body is a text template or an exclusive seal. An empty body clears its owned range. A nonempty body rendering empty is an error. Standalone block tag lines are removed, and the content newline preceding a standalone closing tag remains.

| Mode | Required arguments | Optional arguments |
| --- | --- | --- |
| `patch "between"` | `after`, `before` | `name` |
| `patch "line"` | `prefix` | `after` and `before` together, `insert_before`, `name` |

Arguments are nonempty string literals. `after` and `before` must be distinct. A name is case-sensitive, unique in the source, and consists of ASCII letters, digits, underscores, and hyphens. A line locator starts with an unindented single-line prefix. The insertion anchor is also an unindented single-line prefix, and neither prefix may start with the other. The body of a line patch must contain neither CR nor LF. Unsupported arguments and invalid body shape produce `E-R1`.

- For `between`, find exactly one occurrence of `after` in the input and exactly one occurrence of `before` after the end of that match. Replace the interval between them, including a zero-length interval. Both markers are retained.
- For `line`, an optional marker pair uses the same search and must bound whole lines. Physical lines are separated by LF, with CRLF recognized as one ending. Match the prefix after leading spaces and tabs. A unique match retains indentation, prefix, and line ending, and replaces its entire suffix. Multiple matches fail. With no match, a prefix found inside a line prevents insertion. Otherwise `insert_before` can insert before exactly one anchor line in scope. The new line uses the anchor's indentation and ending. With an unterminated anchor, use the first ending in the file, or LF if none exists.
- Find every edit on the same original bytes. Edits must be disjoint and preserve all retained locator ranges. Multiple line insertions at one position are applied in source order. Resolve the complete set again on changed output. It must succeed without another byte change.

A missing target, an absent or ambiguous required locator, an invalid search range, intersecting edits, or output that fails to converge produces `E-PA1`. Publication requires the original target digest to still match.

<a id="seals"></a>

## Seals and identities

A `seal` block takes no arguments and holds canonical single-line Base64 of binary age ciphertext. It is the entire file declaration or the exclusive content of a patch body. An invalid block or invalid Base64 produces `E-R1`. An empty seal can be filled explicitly. Planning deployment with an empty seal fails. The presence of any seal makes the entire target private and redacts content and byte length from its detailed diff. Patch locators are public.

New ciphertext uses the nonempty list of valid age X25519 public keys in `recipients`, with duplicates collapsed. Missing recipients prevent new ciphertext. Opening existing ciphertext only needs a matching local identity. Resolve the identity by `AGE_IDENTITY_FILE`, then `identity_file`, then `~/.config/age/keys.txt`. The configured path is absolute or begins with `~/` followed by normal path components. `AGE_PASSPHRASE` supplies the passphrase for an existing passphrase-protected identity. Missing or unusable identity, missing or wrong passphrase, nonmatching identity, unfilled seal, and damaged ciphertext produce distinct `E-E1` reasons.

`init --key` resolves the same identity location. It can create a missing identity privately without replacing another file. An existing unusable identity fails and is kept. If recipients are absent and the identity holds exactly one distinct key, the command binds that public key using the observed configuration. Multiple distinct keys require explicit selection. When recipients already exist the command reports local public keys for an explicit policy change and keeps the configured recipients. A created identity can survive failure to publish recipients and be reused on retry. Changing recipients only affects new ciphertext. A newly created key cannot open old seals addressed to a lost identity.

A successful private publication creates or replaces the entire target with owner read/write access, using mode `0600` on POSIX and a protected owner-access DACL on local NTFS. The planner checks support for private publication. A known unsupported target produces `E-P1` before file publication. Equal bytes with insufficient protection are scheduled for replacement.

| Operation | Selected bytes and effect |
| --- | --- |
| `add --encrypt PATH…` | Adopt existing regular targets into new whole-file seals. Encryption requires recipients and can run without opening an identity. |
| `edit PATH --seal[=NAME]` | Read nonempty hidden input or raw stdin and seal the selected body. |
| `absorb PATH --seal[=NAME]` | Read nonempty bytes from the existing target or its selected range. |
| `edit PATH --rekey` | Open all filled seals in the source and re-encrypt their plaintext for the current recipients. |
| `absorb PATH --rekey` | Collect the existing target bytes for all filled seals and re-encrypt them for the current recipients. The old identity is unnecessary. |
| `edit` or `absorb` with both switches | Force fresh encryption of the selected body using the corresponding input, without opening old ciphertext. |

Explicit sealing and rekeying require one exact source file. Bare `--seal` selects a whole-file seal or the sole patch. `--seal=NAME` selects a named patch. A patch must have a literal body or a seal to be explicitly sealed. Changing a filled seal without `--rekey` opens its old ciphertext to retain it when the plaintext is equal. Ordinary target collection has the same ciphertext retention rule. Output is `sealed` or `unchanged` with the path and the selected patch name when present. Standalone `--rekey` reports `rekeyed` and the path. POSIX supports protected interactive editing of sealed sources. On Windows use the explicit seal or rekey modes.

Changing recipients or rekeying the working tree leaves historical ciphertext in earlier commits. If the old key is unavailable, recovery requires a matching identity, a trusted deployed copy, or the original plaintext.

<a id="deployment"></a>

## Planning and publication

`status`, `diff`, and `apply` share a plan. An existing byte range may be replaced if its contents match the last confirmed bytes or the user selects that exact source with `--overwrite`. A record is valid only under the current local key. A missing, unreadable, or unauthenticated record grants no overwrite permission. A whole-file source can create a missing target. A patch may insert a missing line when its locator admits insertion, or fill an empty interval between matched boundaries. Patch ranges are compared independently, so edits outside those ranges do not block deployment. Identical sealed bytes may still need an owner-only access repair.

A blocked plan writes zero targets and starts no scripts. Publication checks for a free position for creation and the observed full-file digest for replacement. This also applies to `--overwrite`. The digest check and replacing rename are distinct operations. A failure after publication starts retains and reports the completed writes. Only declared destinations enter the plan.

Local records contain keyed fingerprints scoped to the repository, resolved destination, and managed range. Matching observations made by `apply` and successful publications can establish a record. Ordinary `absorb` records the values it successfully represents. Before changing managed contents, the writing command invalidates the old record. If interrupted before the new record is saved, the next run treats differing bytes as unverified. `diff` and `status` only read records. Records live under `tuckin/deployments-v1` in the local data directory. `TUCKIN_STATE_DIR` may select an absolute base outside the repository. A managed destination cannot occupy that state tree.

`add` requires exact, mapped, non-metadata paths with existing regular targets and absent sources. It prepares the batch before publishing. An ordinary `absorb` updates existing sources from targets. It writes back whole literal files and eligible owned patch bytes, preserving locator syntax and ciphertext when unchanged. It skips missing targets and unfilled seals, and reports `unresealed` when a missing target leaves filled ciphertext in place. A body with generated bytes that cannot be represented as literal source is refused with `E-AB1`. A failed preparation publishes nothing. A subsequent publication failure retains the completed source writes.

<a id="scripts"></a>

## Script execution

Scripts are regular files immediately under root `scripts/`. Admitted directories and nonregular entries are errors. The same ignore rules govern admission. Extension matching is case-insensitive.

| Extension | Dispatch |
| --- | --- |
| `.sh` / `.bash` | `sh` / `bash` |
| `.lua` | Standalone `lua` |
| `.ps1` | PowerShell 7 via `pwsh -NoProfile`, adding `-ExecutionPolicy Bypass` on Windows |
| `.py` | An absolute active virtual environment's Python 3 first, then `python3` and `python` from executable search, then local Python 3 installations reported by the Windows launcher |
| Other | Direct execution on POSIX. Windows accepts `.exe` and uses its command processor for `.cmd` and `.bat`. |

The explicit runtimes read the script file directly and are selected by extension independently of shebangs. On Windows the selected shells must handle native paths. A required runtime that cannot be resolved fails before file publication with `E-P1`. After file publication, scripts run in filename byte order in the repository root, with the invoking account's permissions and environment. A script failure stops execution and keeps effects already completed. File selectors leave the script phase enabled. `--no-scripts` skips even reading the scripts directory.

A file named with the `gate-` prefix can reuse a successful run at the same repository position, Git state, script name, dispatch, and rendered bytes. Only misses need a runtime. New successes are recorded after the entire script phase succeeds. An unavailable, invalid, or unwritable cache can cause reruns. `TUCKIN_CACHE_DIR` can select a cache directory outside the repository. Ordinary scripts run on every enabled apply.

<a id="cli"></a>

## Commands and diagnostics

| Command | Scope and result |
| --- | --- |
| `init [--from SOURCE]` | Initialize an empty working directory. Plain `init` creates `tuckin.toml`. Only `--from` requires the `git` executable. It accepts a Git URL or local repository path, delegates cloning and credentials to Git, and validates the cloned layout before publication. `--from` and `--key` are mutually exclusive. |
| `init --key` | Use the nearest existing repository. Prepare the local identity and initialize or inspect recipient policy as described under [Seals and identities](#seals). |
| `status [PATH…]` | Emit `create`, `overwrite`, `repair`, `blocked`, or `unchanged` and a spelling per TSV row. Additional `?` rows list undeclared neighbors. A nonzero ignored count describes the full scan. |
| `diff [PATH…]` | Emit pending and blocked rows as TSV when redirected. Terminal output counts changes and samples up to six paths. No differences yield empty output. |
| `diff --patch PATH` | Preview one exact source. A textual preview is bounded to 24 body lines and 76 columns, with limits of 64 KiB and 4096 lines per side and 65,536 comparison cells after common edges are removed. It omits binary, sealed, and over-limit content. |
| `apply [--no-scripts] [--overwrite] [--list \| --patch] [PATH…]` | Publish files, then run enabled scripts. `--overwrite` requires exact file paths. Terminal output samples up to six completed publications. Redirected default output keeps `created`, `overwritten`, `unchanged`, and `bytes`. `--list` emits `create`, `overwrite`, or `repair` and the source path for each completed publication. `--patch` shows the bounded applied change for one exact file. On failure, any completed publications are reported on stderr. |
| `add [--encrypt] PATH…` | Adopt existing targets into new sources. |
| `edit [PATH…]` | Invoke the executable named by `EDITOR` without parsing shell arguments. The default path is `tuckin.toml`. Explicit seal modes are defined above. |
| `absorb [PATH…]` | Collect target changes into sources. Explicit seal modes are defined above. |

With no paths, `status`, `diff`, `apply`, and `absorb` select the entire admitted repository. Results go to stdout and diagnostics go to stderr. Success returns 0. A successful `diff` with differences, including blocked ones, returns 1. Usage and operation failures return 2. A coded diagnostic carries a subject and the fields `missing`, `current`, `why`, and `fix`. Some usage or I/O failures have an uncoded message.

| Code | Failure family |
| --- | --- |
| `E-G1` | Repository marker, configuration, tags, or data |
| `E-A1` / `E-A4` | Spelling, translation, or anchor shape |
| `E-I1` / `E-T1` | Conflicting placement or required regular position |
| `E-R1` / `E-PA1` | Source form or rendering, or a patch that cannot be located or applied |
| `E-AB1` | Target bytes with no valid source representation |
| `E-E1` / `E-P1` | Seal or identity failure, or a missing platform capability |
