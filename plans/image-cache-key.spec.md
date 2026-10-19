# Image cache key

Status: approved 2026-10-19. This document supersedes the "notes keyed by TOML config path" decision in `plans/checkpoint-images.plan.md` and the matching text in `plans/checkpoint-images.spec.md`. The design was de-complected on the same day; the memo recording what changed is the final section.

## Terms

- Checkpoint: the nearest ancestor of HEAD that touches a file listed in `[checkpoint].build_inputs`. In latest-commit mode, with no `[checkpoint]` section, the checkpoint is HEAD.
- Base image: the provider image built from the checkpoint's exported tree plus the recipe.
- Thin diff: the patch from the checkpoint to the working tree, applied on top of the base image on every run.
- Note: the JSON object stored in `refs/notes/offload-images` on the checkpoint commit. It maps an entry name to a cached base image.
- Recipe: everything other than the checkpoint tree that determines the base image.
- False reuse: serving a cached image that does not match what the current inputs would build. False invalidation: rebuilding when nothing relevant changed.

## Principle

False reuse is never acceptable. Offload's cache is part of correct operation: a stale image makes tests report results for an environment nobody asked for. Every trade-off below is resolved toward rebuilding.

## Problem

Today the note key is the pair (checkpoint SHA, repo-relative config path). The config path stands in for the recipe, and it is wrong in both directions.

1. False reuse. Editing a field that shapes the base image, such as `sandbox_init_cmd` or `provider.dockerfile`, without touching a `build_inputs` file leaves both halves of the key unchanged. The next run hits the note and thin-diffs the old image. The new value is never used and nothing is logged. Reproduced against Offload 0.9.14 for a committed `sandbox_init_cmd` edit, an uncommitted edit, a `provider.dockerfile` path change, and latest-commit mode.
2. False invalidation. Configs that share every base-image input but differ in `[framework]`, `[report]`, or `sandbox_project_root` are keyed apart and each builds its own base. In `imbue-ai/default-workspace-template` the three Modal configs rebuilt the same base three times on every checkpoint commit: 155 of 191 noted checkpoints carry three entries, 521 base builds for 191 checkpoints, and about 19 minutes of image preparation per checkpoint commit where one build plus two thin diffs takes about 9.
3. Working-tree leak. In a checkpoint build the Modal script resolves `--include-cwd`, each `--copy-dir`, and `.dockerignore` against its working directory, which is the live working tree rather than the exported checkpoint tree. The gitignored but not dockerignored files that `export_tree` copies in come from the live tree as well. A cached "checkpoint" image therefore depends on what the first builder had on disk, and later thin diffs cannot correct it.

Problems 1 and 2 have one cause and one fix. Problem 3 is independent of the key and is fixed alongside it because it is also false reuse.

## What identifies a cached base image

A cached base image is identified by the checkpoint SHA and the recipe. The recipe is stored in full in the note entry; lookup compares it for exact equality. The entry's name is `recipe:` followed by the first 16 hex characters of the SHA-256 of the recipe's JSON, and serves only as a label.

### Recipe

| Field | Source | Why it is in |
|---|---|---|
| `format` | constant in Offload, starts at 1 | Version of the Rust side of the pipeline. Bumped when export rules or argument passing change what an unchanged recipe builds |
| `builder` | tagged by `[provider].type` | What turns the recipe into an image, and the fields only that builder reads |
| `builder.modal.script` | SHA-256 of the bundled `modal_sandbox.py` | The script assembles the image; a change to it or to its pinned Modal SDK may change the image |
| `builder.modal.dockerfile` | `[provider].dockerfile` | Selects which Dockerfile in the checkpoint tree is built |
| `builder.modal.include_dir` | the invoking directory relative to the repository root when `[provider].include_cwd` is set (empty at the root), otherwise null | The directory added at `/app`, recorded as what enters the image rather than where Offload ran, so one config invoked from two directories cannot share an image |
| `builder.default.prepare_command` | `[provider].prepare_command` | The user's assembler |
| `builder.default.working_dir` | `[provider].working_dir` | Where `prepare_command` runs; relative paths resolve against it |
| `copy_dirs` | `[provider].copy_dirs` followed by the `--copy-dir` flags, in order | Directories added to the image, as the builder receives them. Each entry is `{source, target, content}`; `source` is the repository-relative path of a source inside the repository, whatever spelling and invoking directory named it, and the path as written for one outside; `content` is the source's content digest when the source lies outside the repository (`absent` when it does not exist on disk), otherwise null |
| `sandbox_init_cmd` | `[offload].sandbox_init_cmd` | Runs as the last base-image layer. Empty is normalized to absent |
| `export_extras` | files `export_tree` copies from the live tree | `{paths, content}`: the sorted paths of the gitignored but not dockerignored files that enter the export, and one digest of their content. Null when there are none |
| `escaping_symlinks` | tracked symlinks whose targets lie outside the repository | `{paths, content}`: the sorted repository-relative paths of the tracked symlinks whose targets resolve outside the repository or are missing, and one digest of those targets, a missing one as `absent`. The export carries each link and the builder follows it into the live tree. Null when there are none |

Values are taken verbatim from the loaded config. Environment expansion applies only to `[provider].env`, so no value enters a note that is not already in the committed TOML.

### Not in the recipe

| Field | Phase it feeds | Why it is out |
|---|---|---|
| `post_patch_cmd` | thin diff, every run | Including it would rebuild the base on every edit for nothing |
| `sandbox_repo_root` | thin diff (patch apply root) | Does not change the base image |
| `sandbox_project_root` | test execution (`OFFLOAD_ROOT`) | Never reaches the builder |
| `[provider].env`, `cpu_cores`, `memory_gb`, `experimental_options` | sandbox creation | Never reach the builder |
| `[framework]`, `[groups]`, `[report]`, `[history]`, `max_parallel`, `test_timeout_secs`, `[offload].working_dir` | discovery, scheduling, reporting | Never reach the builder |
| `[checkpoint].build_inputs` | checkpoint selection | For a given checkpoint SHA the image is the same however the SHA was chosen |
| Offload's crate version | none | A release that leaves the pipeline alone builds the same image; `format` and `builder.modal.script` cover the releases that do not |

## Working-tree inputs

The base build is a function of the checkpoint tree and the recipe.

- Rust resolves every path the builder reads against the exported checkpoint tree and passes it as an explicit argument: the Dockerfile, the directory that `include_cwd` adds (the export at the invoking directory's path relative to the repository root), `.dockerignore`, and each copy source. The script reads nothing relative to its own working directory.
- A copy source whose canonical path lies outside the repository has no counterpart in the export. It is passed as given, and its content digest is recorded in its `copy_dirs` entry; a source that does not exist on disk is recorded as `absent`, so its later appearance is a miss.
- The gitignored but not dockerignored files that `export_tree` copies from the live tree are recorded in `export_extras`. For a repository whose `.dockerignore` excludes everything `.gitignore` excludes, the set is empty and costs nothing.
- A tracked symlink is exported as a link, which the builder follows. One whose target resolves inside the repository reads the export's copy of that target. One whose target lies outside the repository reads live content, so those links are recorded in `escaping_symlinks` with one digest of their targets; a dangling link is recorded as `absent`, so its target's later appearance is a miss.

The sandbox ends up with the same files as before: what the live tree used to bake into the base now arrives through the export plus the thin diff.

## Note format

The note stays a flat JSON object. Every value is an entry with a required `image_id` string. New entries add optional fields:

```json
{
  "recipe:3f9c2a7b1e5d4c08": {
    "image_id": "im-6Ys5w652ck375MHjQS6nDR",
    "recipe": {
      "format": 1,
      "builder": {
        "modal": {
          "script": "9b1d...",
          "dockerfile": "system/Dockerfile",
          "include_dir": null
        }
      },
      "copy_dirs": [],
      "sandbox_init_cmd": "default-workspace-template-seed && bash system/scripts/offload_sandbox_init.sh",
      "export_extras": null,
      "escaping_symlinks": null
    },
    "config_path": "offload-modal.toml",
    "offload_version": "0.9.15",
    "built_at": "2026-10-19T22:39:45Z"
  },
  "offload-modal.toml": { "image_id": "im-oGLrFv2W0AcxcUw7rFXIzI" }
}
```

`recipe` is stored inline so `git notes show` says what was compared. `config_path` records which config built the entry and is informational only. The second entry above is a legacy entry written by an older Offload.

Two constraints keep older Offload versions working against new notes: never add a top-level key whose value is not an entry, and never drop `image_id`. An older version that cannot parse a note falls back to a full build on every run for that commit and never writes a note, because the full-build fallback deliberately writes none.

## Lookup and write

- Lookup computes the current recipe, reads the note on the checkpoint, and takes the entry whose stored `recipe` equals it as a JSON value. Entries without a `recipe` are legacy entries and never match.
- On a miss, one log line states how many entries were legacy and how many had a different recipe.
- After a base build, the entry is written under its recipe name with the merge semantics of today, and pushed. An existing entry with the same name is replaced.
- `--no-cache` skips lookup and never writes, as today.

## Migration

No config changes. Every existing TOML keeps its meaning. The first run after upgrading misses once per distinct recipe per active checkpoint and rebuilds; from then on configs with the same recipe share one base image. Legacy entries remain in the notes and are ignored. Older Offload versions keep reading and writing their path-keyed entries beside the new ones. An older version that writes a note on a commit strips the new fields from every entry on that commit, which turns the recipe-keyed entries into legacy entries; the next run of the new version rebuilds once.

## Debuggability

`offload checkpoint-status` prints the current recipe name and, for each entry on the checkpoint, whether it matches, which recipe fields differ, or that it is a legacy entry, so a reader sees "sandbox_init_cmd differs" rather than "none".

## Providers

- Local builds no images and is unaffected.
- Default uses the same pipeline and the same lookup; its builder variant carries `prepare_command` and `working_dir`.
- Modal as described above.

## Costs

- One rebuild per distinct recipe per active checkpoint on the first run after upgrading. The reporting repository pins its Offload version, and bumping it touches a listed build input, so for them the miss coincides with a new checkpoint.
- A rebuild whenever a recipe field changes, including on releases that change the bundled script. These are real inputs.
- For the reporting repository, one base build per checkpoint commit instead of three.

## Implementation plan

Each item is one bead and one commit.

1. `Recipe` type with the `builder` variant, JSON serialization, the name digest, the `sha2` dependency, and unit tests including a fixed-recipe digest.
2. `ImageEntry` optional fields; tests that a legacy note parses and that a new note deserializes under the old entry shape.
3. Lookup by recipe equality and write under the recipe name; legacy entries never match; one log line on a miss; `Recipe` threaded through base resolution. Tests: a recipe change on the same checkpoint misses; two config paths with one recipe hit; a stored recipe with an extra field does not match. Doc comments in `src/config/schema.rs`, `src/git.rs`, and `src/image_cache.rs` updated.
4. `offload checkpoint-status`: recipe name, per-entry verdicts with differing fields, legacy entries flagged.
5. Builder paths resolved by Rust against the export and passed explicitly; script arguments replace working-directory reads. Test: the file set the sandbox receives is unchanged for `include_cwd` and relative `copy_dirs`.
6. Content digests for copy sources outside the repository and for `export_extras`. Tests: a changed artifact changes the recipe; an unchanged one does not.

## Simplification memo

This memo records the braids found in the first approved draft of this document and how each was cut.

Each entry: braid, its cost in the shipped system, the cut, what enforces the invariant afterward, and the precondition the cut rests on.

1. **Lookup addressed by a key prefix and then re-verified against the stored recipe.** Two mechanisms answered one question, "was this entry built from my recipe", and the second existed only because the first is probabilistic, which forced a special case for "key matches, recipe differs". Cut: lookup matches on exact equality of the stored recipe JSON with the current recipe JSON; the digest is only the entry's name. Survivor: JSON equality, which also makes an entry with an extra or missing field unequal, so no Offload version can match an entry whose recipe it does not fully understand. Precondition: the recipe is stored inline in full, which the original already required.

2. **Canonical serialization rules and a golden digest carried as design requirements.** Field order and null handling became invariants every future edit to the struct had to respect, when they only ever mattered for addressing. Cut: with equality matching, serialization affects only the entry name. The name is the SHA-256 of the recipe serialized through `serde_json::Value`, whose object keys are sorted because the crate is built without `preserve_order`, so a Rust field reorder cannot change the name either. The fixed-recipe digest test becomes an ordinary unit test on the helper. Survivor: equality matching never reads the name.

3. **`copy_dirs` and `runtime_copy_dirs` as two fields.** Provenance was encoded in the recipe for one input, the ordered list of copies the builder receives. Cut: one `copy_dirs` list, config entries followed by `--copy-dir` entries. Survivor: the list is exactly the builder's argument order. Precondition verified: both providers append config entries first and runtime entries second (`src/provider/modal.rs:92-99`, `src/provider/default.rs:76-83`).

4. **`tree_extras` fused two independent live-tree inputs into one opaque digest.** A changed copy source outside the repository and a changed gitignored file copied into the export both surfaced as "tree_extras differs", and `git notes show` could not say which files were hashed. Cut: a copy source outside the repository carries its own content digest beside its path; the gitignored extras become `export_extras`, the sorted path list plus one content digest. Survivor: both digests still enter the recipe, so either change is a miss. Precondition: a source is outside the repository exactly when its canonical path is not under the repository root.

5. **Provider-specific fields flattened into one struct with nulls.** A Modal recipe carrying `prepare_command` was representable. Cut: `builder` is a tagged variant; `modal` carries the bundled script digest, `dockerfile`, and `include_cwd`, and `default` carries `prepare_command` and `working_dir`. Survivor: the type. Precondition verified: `dockerfile` and `include_cwd` exist only on the Modal config and `prepare_command` and `working_dir` only on the default config (`src/config/schema.rs:208`, `:217`, `:277`, `:319`).

6. **`assembler` named the Modal script for every provider.** A release that edits the Modal script would have invalidated default-provider caches whose base build never runs it. Cut: the script digest lives in the `modal` variant only; for `default`, `prepare_command` is the assembler. Survivor: the variant. Precondition verified: the default provider's base build runs only `prepare_command` (`src/provider/default.rs:104-127`); the bundled script enters only its thin-diff step, which is never cached.

7. **The builder resolved sources through an implicit channel, the process working directory, so the fix moved that directory and separately rewrote absolute paths.** Three script code paths read the working directory, the `os.path.isfile(dockerfile_path)` check read it while the Dockerfile itself was read through `--context-dir`, and a rule about "the same relative subdirectory" had to be stated and preserved. Cut: Rust resolves every path the builder reads against the export and passes it explicitly; the script reads nothing relative to its working directory. Survivor: explicit arguments. Precondition verified: the script is embedded in the binary (`src/bundled.rs:12`), so its argument protocol changes with the binary.

8. **`format` justified as covering "changes the other fields cannot see".** Cut: `format` is the version of the Rust side of the pipeline, bumped when export rules or argument passing change what an unchanged recipe builds. Wording only.

Preserved as intentional: `format` as the declarative single site for Rust-side pipeline changes; `config_path`, `offload_version`, and `built_at` as provenance that nothing reads; the merge-on-write and force-push of notes, which predate this design; and the two compatibility constraints, never add a top-level key that is not an entry and never drop `image_id`, which are the sole enforcement of old-version compatibility.

Inherent complexity left alone: `export_tree` copies gitignored but not dockerignored files from the live tree because derived artifacts the Docker build needs are not in git. `post_patch_cmd` is the newer answer to that need, and retiring the copy would remove `export_extras` entirely. That is a product change, not a simplification.

Forks, resolved here as recommended and open to the author:

- Retire the gitignored-file copy in favour of `post_patch_cmd`. Not done; `export_extras` stays.
- Match by equality (chosen) or address by key and verify.
- Script digest only in the Modal builder (chosen) or for every provider.

Correctness note routed out of this pass: an older Offload that writes a note on a commit re-serializes every entry through its own entry type and strips the new fields, so recipe-keyed entries on that commit become legacy and the next run of the new version rebuilds. The direction is safe. It is stated under Migration.
