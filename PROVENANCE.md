# Source provenance

This repository's history was extracted with `git subtree split` from
`https://github.com/nonary-streams/amaterasu` at commit
`72eaf63f0a381c1118656d615ae6b4badd50b86f`:

| Source path | Path here | Split commit | Commits |
| --- | --- | --- | --- |
| `modules/encode` | `encode` | `fa67a86b72e6b0afed86b364c6ae37f254b70c18` | 80 |
| `modules/video` | `video` | `54bfc27b6badbf16cba461467b95acbef4d7e08b` | 125 |

The 205 extracted commits keep their original author, message and file
history, and each split commit's tree equals its source subtree. The earlier commits on `main` were a source snapshot from 2026-10-25 with two experiments written against an SDK hook interface that no longer exists; they remain in this branch's history.

Extraction added the package identity in each `Cargo.toml`, licensing, the
standalone build helper, continuous integration, the release workflow and
`metadata/module.json`.
