# Remote git backup example

Sample scripts + unit files that push an ai-memory install to a remote git
repository on a daily cadence, keeping an online `ai-memory backup` snapshot
on a second location for the SQLite state that the mirror repo deliberately
excludes. Models remain redownloadable and are not backed up.

See [`docs/backup.md`](../../backup.md) for the walkthrough. The pieces in this
directory are:

- `ai-memory-snapshot.sh`: the rsync + git push + online tarball + optional
  encrypted OneDrive loop. It is configurable through environment variables
  including `REPO_DIR`, `DATA_SRC`, `CFG_SRC`, `TARBALL_DIR`, `ONEDRIVE_DIR`,
  `LOG_DIR`, and `SECRET_EXCLUDES` with sensible defaults for a single-user
  Linux/WSL/macOS install.
- `ai-memory-backup.service`: a `systemd --user` oneshot that runs the
  script. `%h` expands to the user's home directory.
- `ai-memory-backup.timer`: a daily user timer with a small randomised delay
  and `Persistent=true` so the missed run after a reboot still fires.
- `.gitignore`: the exclusion list to drop into the root of the mirror
  repository, as a second layer behind the rsync/tar exclude lists in the
  script itself.

## Quickstart

```bash
# 1. Create a private repository on your git host (e.g. github.com), then:
mkdir -p ~/ai-memory-backup
cd ~/ai-memory-backup
git init
git remote add origin git@github.com:<you>/ai-memory-backup.git
cp .../docs/examples/backup/.gitignore .
git add .gitignore
git commit -m "initial: ignore secrets, DB, models, logs"
git branch -M main
# push once so `snapshot.sh` has a remote to push to
git push -u origin main

# 2. Drop the snapshot script somewhere on $PATH.
install -Dm755 \
    .../docs/examples/backup/ai-memory-snapshot.sh \
    ~/.local/bin/ai-memory-snapshot.sh

# 3. Install the unit files.
install -Dm644 \
    .../docs/examples/backup/ai-memory-backup.service \
    ~/.config/systemd/user/ai-memory-backup.service
install -Dm644 \
    .../docs/examples/backup/ai-memory-backup.timer \
    ~/.config/systemd/user/ai-memory-backup.timer

# 4. Enable + start.
systemctl --user daemon-reload
systemctl --user enable --now ai-memory-backup.timer

# 5. Trigger one snapshot to confirm the plumbing works.
systemctl --user start ai-memory-backup.service
journalctl --user -u ai-memory-backup.service --since '5 minutes ago'
```

## Encrypted OneDrive copy

OneDrive support targets a directory that is already mounted or synchronized by
an official OneDrive client. Configure both the destination and a passphrase;
no passphrase is embedded in the script, archive name, or logs.

```bash
# Create an owner-only passphrase file. Store a recovery copy in 1Password.
install -d -m700 "$HOME/.config/ai-memory-backup"
umask 077
openssl rand -base64 48 > "$HOME/.config/ai-memory-backup/passphrase"
chmod 600 "$HOME/.config/ai-memory-backup/passphrase"
# Store a recovery copy in 1Password. Do not put this file in OneDrive or git.

# These exports apply only when you run ai-memory-snapshot.sh directly.
export ONEDRIVE_DIR="$HOME/OneDrive/Backups/ai-memory"
export AI_MEMORY_BACKUP_PASSPHRASE_FILE="$HOME/.config/ai-memory-backup/passphrase"
```

For the supplied systemd timer, use a persistent user-service drop-in; shell
exports are not inherited by a running user manager:

```ini
# ~/.config/systemd/user/ai-memory-backup.service.d/onedrive.conf
[Service]
Environment=ONEDRIVE_DIR=%h/OneDrive/Backups/ai-memory
Environment=AI_MEMORY_BACKUP_PASSPHRASE_FILE=%h/.config/ai-memory-backup/passphrase
```

```bash
systemctl --user daemon-reload
systemctl --user start ai-memory-backup.service
journalctl --user -u ai-memory-backup.service --since '5 minutes ago'
```

The script keeps the plaintext online snapshot in `TARBALL_DIR`, encrypts it
with GnuPG AES-256 in a private staging directory, then atomically renames the
finished `.gpg` archive into `ONEDRIVE_DIR`. It refuses paths where the plaintext
tarball or passphrase file would sync to OneDrive or be mirrored to git. A
configured OneDrive failure makes the scheduled run fail, but the local snapshot
remains available. Leaving `ONEDRIVE_DIR` unset preserves the original local +
private-git behavior.

To decrypt for recovery, stop ai-memory first and never decrypt into a synced
folder:

```bash
gpg --batch --yes --pinentry-mode loopback \
    --passphrase-file "$HOME/.config/ai-memory-backup/passphrase" \
    --output /secure/local/ai-memory-restore.tar.gz \
    --decrypt "$HOME/OneDrive/Backups/ai-memory/ai-memory-YYYY-MM-DD.tar.gz.gpg"
```

Then follow the documented `ai-memory restore --from` procedure against an
isolated or stopped data directory.

## Non-systemd equivalents

The script is a plain bash program; anything that runs a shell command on a
schedule can drive it:

- **macOS launchd:** wrap `ai-memory-snapshot.sh` in a `.plist` under
  `~/Library/LaunchAgents/` with a `StartCalendarInterval` block.
- **Windows Task Scheduler:** run the script inside WSL via `wsl -e bash -lc
  'ai-memory-snapshot.sh'` on a daily trigger. On native Windows use the
  PowerShell equivalent of the rsync + `git add/commit/push` + tar sequence.
- **Docker sidecar:** a small cron container mounted at the ai-memory data
  volume, running the same script.

## Security note

The remote is your own channel, not ai-memory's. Follow the existing
[`SECURITY.md`](../../../SECURITY.md) guidance ("Remote sync security"): use a
private repository, a fine-grained token scoped to that one repo (or an SSH
deploy key), and rotate credentials the same way you rotate any other
long-lived push credential. The script never commits files matched by
`SECRET_EXCLUDES` (default: `*.env`, `auth.json`, `.secrets`, `*.pem`, `*.key`, `*.crt`); the mirror
repo's `.gitignore` catches anything the rsync exclude missed. The separate
online snapshot may include `<data_dir>/config.toml`, including secrets stored
inline there, and always contains private memory data. The git mirror can also
include inline secrets from the copied config directory because filename globs
cannot redact TOML values. Inspect the local and git outputs and protect them as
secret-bearing artifacts. The OneDrive copy is encrypted, but its confidentiality
still depends on keeping and recovering the passphrase; do not place the
passphrase file, decrypted archive, or recovery instructions containing the
actual secret in OneDrive or git.
