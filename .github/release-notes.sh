#!/usr/bin/env bash
# Generate professional GitHub release notes for a tag: a changelog grouped
# by conventional-commit type (feat/fix/…), plus download / install / updater
# sections and a Full Changelog compare link.
#
# Usage: release-notes.sh <tag>     # prints markdown to stdout
# Needs full history + tags (checkout with fetch-depth: 0).
set -euo pipefail

TAG="${1:?usage: release-notes.sh <tag>}"
REPO="${GITHUB_REPOSITORY:-chats-is/termory}"
# Nearest tag reachable from the tag's parent = the previous release.
PREV="$(git describe --tags --abbrev=0 "${TAG}^" 2>/dev/null || true)"
RANGE="${PREV:+$PREV..}${TAG}"

# Emit one "### Title" section listing every commit of the given conventional
# type in RANGE (scope dropped, leading "- " bullet). Silent if none.
emit() {
  local prefix="$1" title="$2" lines
  lines="$(git log "$RANGE" --no-merges --pretty='%s' \
    | { grep -E "^${prefix}(\([^)]+\))?!?: " || true; } \
    | { grep -vE '^chore\(release\): bump' || true; } \
    | sed -E "s/^${prefix}(\([^)]*\))?!?: //" \
    | sed -E 's/^/- /')"
  if [ -n "$lines" ]; then
    printf '### %s\n%s\n\n' "$title" "$lines"
  fi
}

emit feat     '✨ Features'
emit fix      '🐛 Bug Fixes'
emit perf     '⚡ Performance'
emit refactor '♻️ Refactoring'
emit docs     '📝 Documentation'
emit test     '✅ Tests'
emit build    '📦 Packaging'
emit ci       '🔧 CI'
emit chore    '🧹 Maintenance' # `chore(release): bump` filtered out inside emit()

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

if [ -n "$PREV" ]; then
  printf '\n**Full Changelog**: https://github.com/%s/compare/%s...%s\n' "$REPO" "$PREV" "$TAG"
fi
