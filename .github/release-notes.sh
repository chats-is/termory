#!/usr/bin/env bash
# Generate release notes for a tag.
#
# Two outputs, one source of truth for the changelog:
#   release-notes.sh <tag>              # FULL notes for the GitHub Release page
#                                       # (changelog + downloads/install/updater)
#   release-notes.sh <tag> --changelog  # changelog ONLY — fed to latest.json's
#                                       # `notes`, rendered in the in-app
#                                       # UpdateDialog (no download tables /
#                                       # xattr steps / commit-hash list)
#
# Needs full history + tags (checkout with fetch-depth: 0).
set -euo pipefail

TAG="${1:?usage: release-notes.sh <tag> [--changelog]}"
MODE="${2:-full}"
REPO="${GITHUB_REPOSITORY:-chats-is/termory}"
# Nearest tag reachable from the tag's parent = the previous release.
PREV="$(git describe --tags --abbrev=0 "${TAG}^" 2>/dev/null || true)"
RANGE="${PREV:+$PREV..}${TAG}"

# Emit one "### Title" section listing every commit of the given conventional
# type in RANGE (scope dropped, leading "- " bullet). Silent if none.
#
# The version-bump commit is dropped: it is the release itself, not a change in
# it, and with its `chore(release): ` prefix stripped it rendered as a bare
# `- v1.4.2` bullet under Maintenance. Match the WHOLE scope, not a message
# body — this filter used to read `^chore\(release\): bump` while every release
# commit here has been `chore(release): vX.Y.Z` since at least v1.3.2, so it
# never once matched and every published release carried the stray line.
# Dropping the only member of a section removes the section too (`emit` prints
# nothing for an empty list), and a release with nothing else in it falls back
# to the "Maintenance and internal improvements." line below.
emit() {
  local prefix="$1" title="$2" lines
  lines="$(git log "$RANGE" --no-merges --pretty='%s' \
    | { grep -E "^${prefix}(\([^)]+\))?!?: " || true; } \
    | { grep -vE '^chore\(release\):' || true; } \
    | sed -E "s/^${prefix}(\([^)]*\))?!?: //" \
    | sed -E 's/^/- /')"
  if [ -n "$lines" ]; then
    printf '### %s\n%s\n\n' "$title" "$lines"
  fi
}

# The changelog — the ONLY part shared by both outputs.
emit_changelog() {
  emit feat     '✨ Features'
  emit fix      '🐛 Bug Fixes'
  emit perf     '⚡ Performance'
  emit refactor '♻️ Refactoring'
  emit docs     '📝 Documentation'
  emit test     '✅ Tests'
  emit build    '📦 Packaging'
  emit ci       '🔧 CI'
  emit chore    '🧹 Maintenance' # the release commit itself is dropped in emit()
}

# --changelog: just the changelog, for the in-app updater dialog. Command
# substitution already strips trailing blank lines; empty (no conventional
# commits) falls back to a short line so the dialog still shows something.
if [ "$MODE" = "--changelog" ]; then
  body="$(emit_changelog)"
  if [ -n "$body" ]; then
    printf '%s\n' "$body"
  else
    printf 'Maintenance and internal improvements.\n'
  fi
  exit 0
fi

# Default: FULL notes for the GitHub Release page.
emit_changelog

cat <<'MD'
### 📦 Downloads
| Platform | File |
|---|---|
| macOS · Apple Silicon | `Termory_*_aarch64.dmg` |
| macOS · Intel | `Termory_*_x64.dmg` |
| Windows | `Termory_*_x64_en-US.msi` · `Termory_*_x64-setup.exe` |
| Linux | `Termory_*_amd64.AppImage` · `*_amd64.deb` · `*_x86_64.rpm` |

### 🍎 macOS — first launch
Builds are not Apple-notarized, so macOS quarantines the download and may say
*"Termory is damaged and can't be opened"* (common on Apple Silicon). The app
is fine — clear the quarantine flag once, then open it normally:

```sh
xattr -dr com.apple.quarantine /Applications/Termory.app
```

(Drag Termory into **Applications** first; adjust the path if it lives
elsewhere.) Windows SmartScreen: **More info → Run anyway**.

### 🔄 Auto-update
The in-app updater (**Settings → Check for updates**) reads the signed
`latest.json` attached below. Auto-update applies to installs on **v0.2.6 or
later**; earlier builds must be updated manually once.
MD

# Full commit list (not just a compare link) so the release page shows the
# actual records: linked short hash + subject, newest first.
if [ -n "$PREV" ]; then
  printf '\n### 📜 Full Changelog\n'
  git log "$RANGE" --no-merges --pretty="- [\`%h\`](https://github.com/$REPO/commit/%H) %s"
  printf '\n**Compare**: https://github.com/%s/compare/%s...%s\n' "$REPO" "$PREV" "$TAG"
fi
