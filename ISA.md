---
name: "Encrypted ai-memory backup destinations"
task: "Add encrypted OneDrive tarball support to the ai-memory backup example"
slug: encrypted-ai-memory-onedrive-backup
phase: complete
progress: 7/7
started: 2026-11-24
updated: 2026-11-24
principal_stated_goal: "build — a tar with a secret passphase in onedrive too; local-first, then github repo"
principal_stated_goal_locked: 2026-11-24
principal_stated_goal_source: prompt
---

## Ideal State

### L3

A scheduled ai-memory run always creates a consistent local backup, mirrors selected durable text to a private git repository, and leaves a passphrase-encrypted tarball in a configured OneDrive directory without exposing the passphrase in files, logs, arguments, or git.

### L2

The existing backup example remains simple and local-first while gaining:

- an encrypted archive destination for OneDrive;
- safe passphrase loading from an owner-only file or secret environment variable;
- independent failure reporting for git, encryption, and upload;
- retention of encrypted and plaintext local artifacts according to configuration;
- documented setup, restore, and security boundaries.

## Criteria

- [x] ISC-1: The snapshot flow creates the online ai-memory tarball before any remote-copy step. Evidence: `tests/backup_snapshot.sh` records eight `ai-memory backup` calls and decrypts the resulting OneDrive archive byte-for-byte.

- [x] ISC-2: A configured OneDrive destination receives only encrypted archive bytes, never plaintext. Evidence: integration test asserts no `.tar.gz` exists in fake OneDrive while the `.gpg` archive decrypts to the local snapshot.

- [x] ISC-3: Encryption reads its passphrase from `AI_MEMORY_BACKUP_PASSPHRASE_FILE` or `AI_MEMORY_BACKUP_PASSPHRASE`, never command-line arguments or logs. Evidence: script uses `--passphrase-file` or a stdin file descriptor; secret values are never echoed or interpolated into process arguments.

- [x] ISC-4: The script refuses unsafe passphrase files, missing encryption tooling, missing OneDrive destinations, and path overlap with synced or mirrored trees. Evidence: negative tests pass for unsafe permissions, missing secrets, and source/destination overlap; no remote plaintext or mirrored secret is produced.

- [x] ISC-5: Existing local and private-git behavior still works when OneDrive is not configured. Evidence: first four integration phases pass with `ONEDRIVE_DIR` unset, including failed-push retry behavior.

- [x] ISC-6: Encrypted-copy retention and local retention are independently configurable. Evidence: plaintext and encrypted fixtures at different ages prove only the archive past its own threshold is pruned.

- [x] ISC-7: README and systemd example document OneDrive setup, secret storage, restore, and the fact that passphrase is required for decryption. Evidence: example README and service include the destination, owner-only passphrase file, 1Password recovery note, decryption command, and retention settings.

## Anti-criteria

- Passphrases, plaintext archives, bearer tokens, provider secrets, or decrypted memory are committed to git.
- OneDrive becomes a dependency of local backups: its failure must not delete or corrupt the local tarball.
- The script copies a live SQLite file instead of using `ai-memory backup`.

## Test Strategy

| ISC | Probe |
|---|---|
| ISC-1 | `bash tests/examples/backup/ai-memory-snapshot.test.sh` |
| ISC-2 | Same integration suite, fake OneDrive directory |
| ISC-3 | Same suite plus grep for secret exposure |
| ISC-4 | Negative-path cases in same suite |
| ISC-5 | Baseline case in same suite |
| ISC-6 | Retention fixtures in same suite |
| ISC-7 | `bash -n` and documented command/path readback |

## Out of Scope

- Automatically creating a GitHub repository or configuring GitHub credentials.
- Uploading through the Microsoft Graph API; this version targets a locally mounted/synced OneDrive folder.
- Storing the passphrase in 1Password automatically.
- Deploying or scheduling this host configuration.

## Decisions

- 2026-11-24: Use GPG symmetric encryption in the backup example because it is widely available and avoids embedding passphrases in process arguments by reading from file descriptor 3/stdin.
- 2026-11-24: OneDrive means a configured local mount/sync directory (`ONEDRIVE_DIR`), which the script copies encrypted archives into atomically.

## Status

- `bash -n docs/examples/backup/ai-memory-snapshot.sh tests/backup_snapshot.sh` passed.
- `shellcheck docs/examples/backup/ai-memory-snapshot.sh tests/backup_snapshot.sh` passed.
- `bash tests/backup_snapshot.sh` passed.
- `cargo test -p ai-memory-cli repo_layout -- --nocapture` passed (5/5 selected tests).
- `git diff --check` passed.
- Acceptance: real local workflow exercised through fake git remote and fake OneDrive sync directory; encrypted archive decrypted byte-for-byte to the local online snapshot; overlap, unsafe-secret, missing-secret, inherited-config, and independent-retention cases passed (2026-11-24).
- Review mitigation: addressed Sourcery comments 1–13 and CodeRabbit's inherited-environment, private-staging, and systemd findings.
