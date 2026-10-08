# Justfile for ai-memory repository management
# Run `just` or `just --list` to view available recipes

set shell := ["bash", "-euo", "pipefail", "-c"]

# Remote and branch configurations (can be overridden via env vars)
upstream := env('GIT_UPSTREAM', 'upstream')
origin   := env('GIT_ORIGIN', 'origin')
branch   := env('GIT_BRANCH', 'main')

# ─── Default ───────────────────────────────────────────────

# List all available recipes
default:
    @just --list --unsorted

# ─── Sync Workflow ─────────────────────────────────────────

# Safely sync main from upstream (validates tree, fast-forwards origin first, then merges upstream)
[group('sync')]
sync:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "==> Checking working tree status..."
    DIRTY_CHANGES=$(git status --porcelain | grep -v -E '^[?]{2} (justfile|mise\.toml)$' || true)
    if [ -n "$DIRTY_CHANGES" ]; then
        echo "Error: Working directory has uncommitted tracked changes or untracked files:" >&2
        echo "$DIRTY_CHANGES" >&2
        echo "Stash or commit before syncing." >&2
        exit 1
    fi

    CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
    if [ "$CURRENT_BRANCH" != "{{ branch }}" ]; then
        echo "==> Switching branch: $CURRENT_BRANCH -> {{ branch }}"
        git checkout {{ branch }}
    fi

    echo "==> Fetching remotes ({{ origin }} and {{ upstream }})..."
    git fetch {{ origin }} {{ branch }}
    git fetch {{ upstream }} {{ branch }}

    BEHIND_ORIGIN=$(git rev-list --count HEAD..{{ origin }}/{{ branch }})
    if [ "$BEHIND_ORIGIN" -gt 0 ]; then
        echo "==> Fast-forwarding local '{{ branch }}' to '{{ origin }}/{{ branch }}' ($BEHIND_ORIGIN commits behind)..."
        git merge --ff-only {{ origin }}/{{ branch }}
    fi

    BEHIND_UPSTREAM=$(git rev-list --count HEAD..{{ upstream }}/{{ branch }})
    if [ "$BEHIND_UPSTREAM" -eq 0 ]; then
        echo "==> Local '{{ branch }}' is already up to date with '{{ upstream }}/{{ branch }}'."
    else
        echo "==> Merging '{{ upstream }}/{{ branch }}' ($BEHIND_UPSTREAM commits behind)..."
        if ! git merge {{ upstream }}/{{ branch }}; then
            echo "" >&2
            echo "==> Merge conflict detected!" >&2
            echo "    Fix the conflicts and run 'git commit' to complete the merge," >&2
            echo "    or run 'just abort' to cancel and return to previous state." >&2
            exit 1
        fi
        echo "==> Successfully merged '{{ upstream }}/{{ branch }}'."
    fi

# Sync from upstream and push result to origin
[group('sync')]
sync-push: sync push

# Push local branch to origin remote
[group('sync')]
push:
    @echo "==> Pushing '{{ branch }}' to '{{ origin }}'..."
    git push {{ origin }} {{ branch }}

# Fetch latest refs from both origin and upstream
[group('sync')]
fetch:
    @echo "==> Fetching '{{ origin }}' and '{{ upstream }}'..."
    git fetch {{ origin }}
    git fetch {{ upstream }}

# Abort an in-progress merge if conflicts occur
[group('sync')]
abort:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f .git/MERGE_HEAD ]; then
        echo "==> Aborting in-progress merge..."
        git merge --abort
        echo "==> Merge aborted."
    else
        echo "No merge currently in progress."
    fi

# ─── Status & Inspection ───────────────────────────────────

# Compare commit states between local, origin, and upstream
[group('status')]
status:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "=== Working Tree Status ==="
    git status -s
    echo ""
    echo "=== Remote Tracking (branch: {{ branch }}) ==="
    AHEAD_ORIGIN=$(git rev-list --count {{ origin }}/{{ branch }}..HEAD 2>/dev/null || echo 0)
    BEHIND_ORIGIN=$(git rev-list --count HEAD..{{ origin }}/{{ branch }} 2>/dev/null || echo 0)
    AHEAD_UPSTREAM=$(git rev-list --count {{ upstream }}/{{ branch }}..HEAD 2>/dev/null || echo 0)
    BEHIND_UPSTREAM=$(git rev-list --count HEAD..{{ upstream }}/{{ branch }} 2>/dev/null || echo 0)
    echo "  vs {{ origin }}/{{ branch }}:   $AHEAD_ORIGIN ahead, $BEHIND_ORIGIN behind"
    echo "  vs {{ upstream }}/{{ branch }}: $AHEAD_UPSTREAM ahead, $BEHIND_UPSTREAM behind"

# Show commits in upstream/main that are not in local main
[group('status')]
log-upstream count="10":
    git log --oneline -n {{ count }} HEAD..{{ upstream }}/{{ branch }}

# Show local commits not yet in upstream/main
[group('status')]
log-local count="10":
    git log --oneline -n {{ count }} {{ upstream }}/{{ branch }}..HEAD

# Show diff between local main and upstream/main
[group('status')]
diff-upstream:
    git diff HEAD..{{ upstream }}/{{ branch }}

# ─── Verification ──────────────────────────────────────────

# Run compiler checks across all workspace targets
[group('verify')]
check:
    cargo check --workspace --all-targets

# Run the everyday test suite
[group('verify')]
test:
    cargo t

# Full pipeline: sync from upstream then run cargo check
[group('verify')]
sync-verify: sync check
