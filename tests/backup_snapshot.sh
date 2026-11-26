#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export REPO_DIR="$fixture/mirror" DATA_SRC="$fixture/source" CFG_SRC="$fixture/config"
export LOG_DIR="$fixture/logs" TARBALL_DIR="$fixture/backups"
export PATH="$fixture/bin:$PATH" SNAPSHOT_CALLS="$fixture/calls"
unset ONEDRIVE_DIR AI_MEMORY_BACKUP_PASSPHRASE_FILE AI_MEMORY_BACKUP_PASSPHRASE

mkdir -p "$fixture/bin" "$DATA_SRC/wiki" "$CFG_SRC"
git init -q --bare "$fixture/remote"
git init -q -b main "$REPO_DIR"
git -C "$REPO_DIR" config user.name 'Snapshot test'
git -C "$REPO_DIR" config user.email 'snapshot@example.invalid'
git -C "$REPO_DIR" commit -qm initial --allow-empty
git -C "$REPO_DIR" remote add origin "$fixture/remote"
git -C "$REPO_DIR" push -q origin main
initial="$(git -C "$REPO_DIR" rev-parse HEAD)"
printf 'A page to back up\n' > "$DATA_SRC/wiki/page.md"

cat > "$fixture/remote/hooks/pre-receive" <<'EOF'
#!/bin/sh
exit 1
EOF
cat > "$fixture/bin/ai-memory" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" = --data-dir && "$2" = "$DATA_SRC" && "$3" = backup && "$4" = --to ]]
printf 'backup\n' >> "$SNAPSHOT_CALLS"
printf 'test archive\n' > "$5"
EOF
chmod +x "$fixture/remote/hooks/pre-receive" "$fixture/bin/ai-memory"

first_status=0
bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || first_status=$?
local_head="$(git -C "$REPO_DIR" rev-parse HEAD)"
test "$first_status" -ne 0
test "$local_head" != "$initial"
test "$(git -C "$fixture/remote" rev-parse refs/heads/main)" = "$initial"
first_backups=0
if [[ -f "$SNAPSHOT_CALLS" ]]; then
    first_backups="$(wc -l < "$SNAPSHOT_CALLS")"
fi

rm "$fixture/remote/hooks/pre-receive"
bash "$root/docs/examples/backup/ai-memory-snapshot.sh"
remote_head="$(git -C "$fixture/remote" rev-parse refs/heads/main)"
printf 'First run status: %s; backups on first run: %s\n' "$first_status" "$first_backups"
printf 'Local snapshot: %s; remote after retry: %s\n' "$local_head" "$remote_head"
test "$first_backups" -eq 1
test "$remote_head" = "$local_head"
test "$(git -C "$REPO_DIR" rev-parse HEAD)" = "$local_head"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 2

# A native install can keep its configuration entirely in DATA_SRC. A missing
# default external directory should leave the previous external mirror alone.
default_config_home="$fixture/default-home"
mkdir -p "$default_config_home" "$REPO_DIR/config"
printf '# Configuration stored with the data directory\n' > "$DATA_SRC/config.toml"
printf '# Previously mirrored external configuration\n' > "$REPO_DIR/config/previous.toml"
git -C "$REPO_DIR" add config/previous.toml
git -C "$REPO_DIR" commit -qm 'Mirror separate configuration'
git -C "$REPO_DIR" push -q origin main
if ! env -u CFG_SRC HOME="$default_config_home" bash "$root/docs/examples/backup/ai-memory-snapshot.sh"; then
    echo 'Snapshot failed without a default external configuration directory' >&2
    exit 1
fi
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 3
test -f "$REPO_DIR/config/previous.toml"
cmp "$DATA_SRC/config.toml" "$REPO_DIR/data/config.toml"

# Once the default directory exists, it is still mirrored with --delete.
mkdir -p "$default_config_home/.config/ai-memory"
printf '# Current external configuration\n' > "$default_config_home/.config/ai-memory/current.toml"
env -u CFG_SRC HOME="$default_config_home" bash "$root/docs/examples/backup/ai-memory-snapshot.sh"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 4
test ! -e "$REPO_DIR/config/previous.toml"
cmp "$default_config_home/.config/ai-memory/current.toml" "$REPO_DIR/config/current.toml"

# A typo in an explicit override should still cause the scheduled job to fail.
override_status=0
CFG_SRC="$fixture/absent-override" bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || override_status=$?
test "$override_status" -ne 0
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 5
test -f "$REPO_DIR/config/current.toml"

# OneDrive receives only encrypted bytes and the archive round-trips with the
# passphrase file. Keep the local plaintext archive after the remote copy.
ONEDRIVE_DIR="$fixture/onedrive" bash -c 'mkdir -p "$ONEDRIVE_DIR"'
printf 'a-test-only-passphrase-with-enough-entropy\n' > "$fixture/passphrase"
chmod 600 "$fixture/passphrase"
ONEDRIVE_DIR="$fixture/onedrive" \
AI_MEMORY_BACKUP_PASSPHRASE_FILE="$fixture/passphrase" \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh"
encrypted_list=("$fixture/onedrive"/ai-memory-*.tar.gz.gpg)
test "${#encrypted_list[@]}" -eq 1
encrypted="${encrypted_list[0]}"
test -f "$encrypted"
test -z "$(find "$fixture/onedrive" -maxdepth 1 -name '*.tar.gz' -print -quit)"
case "$(file -b "$encrypted")" in
    *OpenPGP*|*PGP*|*data*) ;;
    *) echo "encrypted output has unexpected type" >&2; exit 1 ;;
esac
gpg --batch --yes --pinentry-mode loopback \
    --passphrase-file "$fixture/passphrase" \
    --output "$fixture/decrypted.tar.gz" --decrypt "$encrypted"
plain_list=("$TARBALL_DIR"/ai-memory-*.tar.gz)
test "${#plain_list[@]}" -ge 1
newest_plain=$(find "$TARBALL_DIR" -maxdepth 1 -name 'ai-memory-*.tar.gz' -type f -printf '%T@ %p\n' | sort -nr | head -1 | cut -d' ' -f2-)
cmp "$newest_plain" "$fixture/decrypted.tar.gz"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 6

# Unsafe passphrase permissions and a missing secret both fail closed while the
# local online backup remains present. Neither run may leave a remote archive.
unsafe="$fixture/unsafe-passphrase"
printf 'unsafe\n' > "$unsafe"
chmod 644 "$unsafe"
rm -f "$encrypted"
unsafe_status=0
ONEDRIVE_DIR="$fixture/onedrive" \
AI_MEMORY_BACKUP_PASSPHRASE_FILE="$unsafe" \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || unsafe_status=$?
test "$unsafe_status" -ne 0
test ! -e "$encrypted"
test -f "$newest_plain"

missing_status=0
ONEDRIVE_DIR="$fixture/onedrive" \
env -u AI_MEMORY_BACKUP_PASSPHRASE_FILE -u AI_MEMORY_BACKUP_PASSPHRASE \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || missing_status=$?
test "$missing_status" -ne 0
test ! -e "$encrypted"
test -f "$newest_plain"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 6

# Reject overlap with OneDrive and mirrored source trees before a backup is made.
overlap_status=0
ONEDRIVE_DIR="$fixture/onedrive" \
TARBALL_DIR="$fixture/onedrive/plain" \
AI_MEMORY_BACKUP_PASSPHRASE_FILE="$fixture/passphrase" \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || overlap_status=$?
test "$overlap_status" -ne 0
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 6

printf 'mirrored secret\n' > "$CFG_SRC/backup-secret"
chmod 600 "$CFG_SRC/backup-secret"
leak_status=0
ONEDRIVE_DIR="$fixture/onedrive" \
AI_MEMORY_BACKUP_PASSPHRASE_FILE="$CFG_SRC/backup-secret" \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh" || leak_status=$?
test "$leak_status" -ne 0
test ! -e "$REPO_DIR/config/backup-secret"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 6
rm -f "$CFG_SRC/backup-secret"

# Retention is independently configurable: the plain fixture is past its
# threshold while the encrypted fixture is between the two thresholds.
old_plain="$TARBALL_DIR/ai-memory-2000-01-01.tar.gz"
between_encrypted="$fixture/onedrive/ai-memory-2000-01-02.tar.gz.gpg"
printf 'old\n' > "$old_plain"
printf 'old encrypted\n' > "$between_encrypted"
touch -d '10 days ago' "$old_plain"
touch -d '5 days ago' "$between_encrypted"
TARBALL_RETENTION_DAYS=7 ONEDRIVE_RETENTION_DAYS=14 \
ONEDRIVE_DIR="$fixture/onedrive" \
AI_MEMORY_BACKUP_PASSPHRASE_FILE="$fixture/passphrase" \
bash "$root/docs/examples/backup/ai-memory-snapshot.sh"
test ! -e "$old_plain"
test -e "$between_encrypted"
test "$(wc -l < "$SNAPSHOT_CALLS")" -eq 7
