#!/usr/bin/env bash
# ai-memory-snapshot.sh
#
# Daily snapshot of an ai-memory single-user install:
#   1. create a consistent online backup (wiki + SQLite + data-dir config),
#   2. optionally encrypt and atomically publish it to OneDrive,
#   3. rsync durable source material into a git mirror and push it.
#
# Assumptions:
#   - You already ran `git init && git remote add origin <url>` inside
#     $REPO_DIR and staged the .gitignore shipped alongside this file.
#   - The user running this script has an ssh-agent or credential helper set
#     up so `git push` is non-interactive.
#   - $DATA_SRC is the ai-memory data dir (`ai-memory status` prints it) and
#     $CFG_SRC is where your provider env-file lives.

: "${REPO_DIR:=$HOME/ai-memory-backup}"
: "${DATA_SRC:=$HOME/.local/share/ai-memory}"
cfg_src_is_default=false
if [[ -z "${CFG_SRC:-}" ]]; then
    CFG_SRC="$HOME/.config/ai-memory"
    cfg_src_is_default=true
fi
: "${TARBALL_DIR:=$HOME/backups/ai-memory}"
: "${LOG_DIR:=$HOME/logs/ai-memory-snapshot}"
: "${TARBALL_RETENTION_DAYS:=30}"
: "${ONEDRIVE_RETENTION_DAYS:=30}"
: "${LOG_RETENTION_DAYS:=30}"
: "${SECRET_EXCLUDES:=*.env auth.json .secrets *.pem *.key *.crt}"
: "${ONEDRIVE_DIR:=}"
: "${AI_MEMORY_BACKUP_PASSPHRASE_FILE:=}"
: "${AI_MEMORY_BACKUP_PASSPHRASE:=}"

set -euo pipefail

stamp=$(date +%Y-%m-%d-%H%M%S)
day=$(date +%Y-%m-%d)
log="$LOG_DIR/ai-memory-snapshot_$(date +%Y%m%d-%H%M%S).log"
mkdir -p "$LOG_DIR" "$TARBALL_DIR"
exec >"$log" 2>&1

lock_dir="${XDG_RUNTIME_DIR:-/tmp}/ai-memory-snapshot.lock"
staging_dir=""
cleanup() {
    if [[ -n "$staging_dir" && -d "$staging_dir" ]]; then
        rm -rf -- "$staging_dir"
    fi
    rmdir "$lock_dir" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM
if ! mkdir "$lock_dir" 2>/dev/null; then
    echo "ERROR: another ai-memory snapshot is already running ($lock_dir)" >&2
    exit 1
fi

path_is_within() {
    local child=$1 parent=$2
    child=$(realpath -m -- "$child")
    parent=$(realpath -m -- "$parent")
    [[ "$child" == "$parent" || "$child" == "$parent/"* ]]
}

source_is_mirrored() {
    local path=$1 source=$2
    [[ -e "$path" || -L "$path" ]] && path_is_within "$path" "$source"
}

echo "=== $(date -Iseconds) ai-memory snapshot start ==="
echo "data_src=$DATA_SRC"
echo "cfg_src=$CFG_SRC"
echo "repo_dir=$REPO_DIR"
echo "tarball_dir=$TARBALL_DIR"
if [[ -n "$ONEDRIVE_DIR" ]]; then
    echo "onedrive_dir=$ONEDRIVE_DIR"
else
    echo "onedrive_dir=(disabled)"
fi

if [[ ! -d "$DATA_SRC" ]]; then
    echo "ERROR: $DATA_SRC does not exist. Aborting." >&2
    exit 1
fi
if [[ ! -d "$REPO_DIR/.git" ]]; then
    echo "ERROR: $REPO_DIR is not a git repository (run 'git init' and 'git remote add origin <url>' first)." >&2
    exit 1
fi
if [[ -n "$ONEDRIVE_DIR" ]]; then
    if [[ ! -d "$ONEDRIVE_DIR" ]]; then
        echo "ERROR: ONEDRIVE_DIR does not exist: $ONEDRIVE_DIR" >&2
        exit 1
    fi
    if path_is_within "$TARBALL_DIR" "$ONEDRIVE_DIR"; then
        echo "ERROR: TARBALL_DIR must not be inside ONEDRIVE_DIR" >&2
        exit 1
    fi
    if [[ -n "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]] && path_is_within "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" "$ONEDRIVE_DIR"; then
        echo "ERROR: passphrase file must not be inside ONEDRIVE_DIR" >&2
        exit 1
    fi
    if [[ -n "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]] && \
        { source_is_mirrored "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" "$DATA_SRC" || \
          source_is_mirrored "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" "$CFG_SRC"; }; then
        echo "ERROR: passphrase file must not be inside mirrored source directories" >&2
        exit 1
    fi
    if ! command -v gpg >/dev/null 2>&1; then
        echo "ERROR: gpg is required when ONEDRIVE_DIR is configured" >&2
        exit 1
    fi
    if [[ -n "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]]; then
        if [[ ! -f "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]]; then
            echo "ERROR: passphrase file does not exist: $AI_MEMORY_BACKUP_PASSPHRASE_FILE" >&2
            exit 1
        fi
        passphrase_mode=$(stat -c '%a' "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" 2>/dev/null || stat -f '%Lp' "$AI_MEMORY_BACKUP_PASSPHRASE_FILE")
        passphrase_permissions=$((8#$passphrase_mode))
        if (( (passphrase_permissions & 077) != 0 )); then
            echo "ERROR: passphrase file must not be group/world accessible" >&2
            exit 1
        fi
        if [[ ! -s "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]]; then
            echo "ERROR: passphrase file is empty" >&2
            exit 1
        fi
    elif [[ -z "$AI_MEMORY_BACKUP_PASSPHRASE" ]]; then
        echo "ERROR: configure AI_MEMORY_BACKUP_PASSPHRASE_FILE or AI_MEMORY_BACKUP_PASSPHRASE" >&2
        exit 1
    fi
fi

# Keep the secret out of all mirror subprocesses. The shell retains a local
# copy for the single GPG invocation below.
passphrase="$AI_MEMORY_BACKUP_PASSPHRASE"
unset AI_MEMORY_BACKUP_PASSPHRASE

echo "--- online data snapshot to local location ---"
tarball="$TARBALL_DIR/ai-memory-$stamp.tar.gz"
ai-memory --data-dir "$DATA_SRC" backup --to "$tarball"
echo "tarball: $(wc -c < "$tarball") bytes $tarball"

onedrive_status=0
onedrive_published=false
if [[ -n "$ONEDRIVE_DIR" ]]; then
    echo "--- encrypt and stage for OneDrive ---"
    encrypted="$ONEDRIVE_DIR/ai-memory-$stamp.tar.gz.gpg"
    staging_dir=$(mktemp -d "$ONEDRIVE_DIR/.ai-memory-backup.XXXXXX")
    chmod 700 "$staging_dir"
    encrypted_tmp=$(mktemp "$staging_dir/archive.gpg.XXXXXX")
    rm -f "$encrypted_tmp"

    if [[ -n "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" ]]; then
        if ! gpg --batch --yes --pinentry-mode loopback \
            --passphrase-file "$AI_MEMORY_BACKUP_PASSPHRASE_FILE" \
            --symmetric --cipher-algo AES256 --output "$encrypted_tmp" "$tarball"; then
            echo "ERROR: archive encryption failed" >&2
            onedrive_status=1
        fi
    elif ! printf '%s' "$passphrase" | gpg --batch --yes \
        --pinentry-mode loopback --passphrase-fd 0 \
        --symmetric --cipher-algo AES256 --output "$encrypted_tmp" "$tarball"; then
        echo "ERROR: archive encryption failed" >&2
        onedrive_status=1
    fi
    unset passphrase

    if (( onedrive_status == 0 )); then
        if ! mv -- "$encrypted_tmp" "$encrypted"; then
            echo "ERROR: publishing encrypted archive failed" >&2
            onedrive_status=1
        elif ! chmod 600 "$encrypted"; then
            echo "ERROR: setting encrypted archive permissions failed" >&2
            onedrive_status=1
        else
            onedrive_published=true
            echo "encrypted archive: $(wc -c < "$encrypted") bytes $encrypted"
        fi
    fi
    rm -rf -- "$staging_dir"
    staging_dir=""
fi

# Build a comma-free rsync exclude list. The .gitignore in the mirror repo
# also keeps derived state out of commits, but excluding here keeps the rsync
# working set small.
rsync_excludes=(
    --exclude=.git/
    --exclude=models/
    --exclude=db/
    --exclude=logs/
    --exclude=.serve.lock
    --exclude=auth-tokens/
    --exclude=auth-token
    --exclude=auth-header
    --exclude=hook-spool/
)
for glob in $SECRET_EXCLUDES; do
    rsync_excludes+=(--exclude="$glob")
done

echo "--- rsync data ---"
rsync -a --delete "${rsync_excludes[@]}" "$DATA_SRC/" "$REPO_DIR/data/"

if [[ "$cfg_src_is_default" == true && ! -e "$CFG_SRC" && ! -L "$CFG_SRC" ]]; then
    echo "no separate configuration directory at $CFG_SRC; skipping its mirror"
else
    echo "--- rsync config (secrets excluded) ---"
    config_excludes=()
    for glob in $SECRET_EXCLUDES; do
        config_excludes+=(--exclude="$glob")
    done
    rsync -a --delete "${config_excludes[@]}" "$CFG_SRC/" "$REPO_DIR/config/"
fi

cd "$REPO_DIR"
git add -A
if git diff --cached --quiet; then
    echo "no changes since last snapshot; skipping commit"
else
    git commit -m "auto: snapshot $day" | tail -3
fi

echo "--- push ---"
push_status=0
timeout 180 git push origin "$(git rev-parse --abbrev-ref HEAD)" 2>&1 | tail -10 || push_status=$?
if (( push_status != 0 )); then
    echo "mirror push failed (status $push_status)" >&2
fi

if [[ "$onedrive_published" == true ]]; then
    echo "--- rotate encrypted OneDrive archives (retention: ${ONEDRIVE_RETENTION_DAYS}d) ---"
    find "$ONEDRIVE_DIR" -maxdepth 1 -name 'ai-memory-*.tar.gz.gpg' -type f \
        -mtime "+${ONEDRIVE_RETENTION_DAYS}" -delete -print
fi

echo "--- rotate local tarballs (retention: ${TARBALL_RETENTION_DAYS}d) ---"
find "$TARBALL_DIR" -maxdepth 1 -name 'ai-memory-*.tar.gz' -type f \
    -mtime "+${TARBALL_RETENTION_DAYS}" -delete -print

echo "--- rotate logs (retention: ${LOG_RETENTION_DAYS}d) ---"
find "$LOG_DIR" -maxdepth 1 -name 'ai-memory-snapshot_*.log' -type f \
    -mtime "+${LOG_RETENTION_DAYS}" -delete -print

if (( push_status != 0 || onedrive_status != 0 )); then
    echo "=== $(date -Iseconds) backup complete; one or more mirror destinations failed ===" >&2
    (( push_status != 0 )) && echo "mirror_push_status=$push_status" >&2
    (( onedrive_status != 0 )) && echo "onedrive_status=$onedrive_status" >&2
    exit 1
fi
echo "=== $(date -Iseconds) ai-memory snapshot done ==="
