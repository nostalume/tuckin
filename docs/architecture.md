# tuckin architecture

[简体中文](architecture.zh-CN.md) · [Getting started](../README.md) · [Behavior specification](spec.md)

Tuckin owns the repository's declarations, which target each entry manages, and where a file or script operation may act. Pulith provides local file access, digests, private staging, and conditional publication. Tuckin supplies the bytes and the conditions for publishing them.

## Target ownership and evaluation

```text
repository declarations + machine context
  → resolve target ownership for the repository
  → evaluate the selected sources and targets
  → plan verdicts, bytes, protection, and predecessor conditions
  → report the plan or publish the selected writes
```

Mappings give each source one destination per admitted platform. Platform and tag rules admit sources and scripts. The scan reads the active tags from the checkout's metadata and uses the same list for admission and public template data. Tags can only exclude paths, so portable mapping conflicts can be decided using each platform's untagged set. Resolving parent directories also detects targets that share a position through aliases.

Placement and selection answer different questions. The whole repository determines which files are managed and whether positions conflict. The selector determines whose contents are evaluated. This lets a single-file preview use the same ownership rules as a full run while reading only that entry's contents.

## From source to publication

Source validation establishes the file form, patch rules, and seal positions. Deployment and write-back share that interpretation. The write-back path uses the declared spans to preserve surrounding source text.

A whole-file declaration supplies the expected bytes. A patch locates all of its ranges in the same original target, preserves its boundaries, and derives the expected bytes from that observation. Because the result depends on the original bytes, patch publication carries a digest of the same input. Pulith checks that predecessor condition before replacement.

The file plan is available to both read-only reports and `apply`. All selected entries are planned before publication. A failed publication stops the batch with earlier writes preserved. `absorb` explicitly captures eligible target changes back into the source.

## Seals and script execution

The repository declares public recipients for new ciphertext. Local identities open existing seals. Key initialization uses the same identity path and parser as decryption. A missing identity is created with private, create-only publication. The first recipient is bound only after the identity exists, using the configuration snapshot the command read. A retry can reuse that identity. A seal contributes bytes and a private publication requirement to the file plan.

Pulith establishes owner-only protection on the staged handle before it writes plaintext, then publishes the staged file. Windows supplies this operation on local NTFS. The planner can reject a known unsupported destination before publication and can schedule protection repair when the bytes already agree. A protected editor session has a separate lifetime that includes the editor and its temporary files.

Scripts share repository admission and public rendering inputs. The extension selects a runtime independently of the host. After cache selection, preparation binds each required runtime to one local executable. A missing runtime stops preparation before file publication. Execution then uses the prepared program and script path in spelling order. The gate digest includes the dispatch rule and rendered bytes, and successes are recorded only after the script phase succeeds.

The [behavior specification](spec.md) defines the command surface, platform dispatch, and observable failures.
