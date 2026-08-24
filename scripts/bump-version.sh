#!/usr/bin/env bash
#
# Bump the project version consistently across Cargo.toml, Cargo.lock and
# debian/changelog, then commit (or amend), tag and push.
#
# The release workflow guards that the git tag, the Cargo version and the
# debian/changelog version all agree. This script sets all three together and
# re-runs that same guard locally before anything is pushed, so a mismatch is
# caught here instead of in CI.
#
# The changelog stanza is written in the Debian policy 5.6 format and validated
# with dpkg-parsechangelog, the canonical parser, so dch is not required.
#
# Options (any prompt is skipped when its flag is given):
#   --version X.Y.Z     Target version to set everywhere.
#   --message TEXT      Changelog bullet (repeatable).
#   --amend             Amend the latest commit instead of creating a new one.
#   --new-commit        Create a new commit instead of amending.
#   --no-push           Do everything locally but do not push.
#   --yes               Assume "yes" for confirmations.
#   --dry-run           Show what would happen without changing anything.
#   -h, --help          Show this help.
set -euo pipefail

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*" >&2; }

usage() { sed -n '2,/^set -euo/{/^set -euo/!p}' "$0" | sed 's/^# \{0,1\}//'; }

VERSION=""
declare -a ENTRIES=()
COMMIT_MODE=""
DO_PUSH=1
ASSUME_YES=0
DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --message) ENTRIES+=("${2:?--message needs a value}"); shift 2 ;;
    --amend) COMMIT_MODE="amend"; shift ;;
    --new-commit) COMMIT_MODE="new"; shift ;;
    --no-push) DO_PUSH=0; shift ;;
    --yes) ASSUME_YES=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

cd "$(git rev-parse --show-toplevel)" || die "not inside a git repository"

for tool in cargo git dpkg-parsechangelog; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done

confirm() {
  # confirm PROMPT [default-yes]
  local prompt="$1" default="${2:-yes}" reply
  if [[ $ASSUME_YES -eq 1 ]]; then return 0; fi
  local hint="[y/N]"; [[ "$default" == "yes" ]] && hint="[Y/n]"
  read -r -p "$prompt $hint " reply || true
  reply="${reply:-$default}"
  [[ "$reply" =~ ^([yY]|yes)$ ]]
}

ask() {
  # ask VARNAME PROMPT DEFAULT
  local __var="$1" prompt="$2" default="${3:-}" reply
  read -r -p "$prompt${default:+ [$default]}: " reply || true
  printf -v "$__var" '%s' "${reply:-$default}"
}

semver_ok() { [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.+][0-9A-Za-z.-]+)?$ ]]; }

cargo_version() { sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1; }

lock_version() {
  awk '/^name = "keylightd"$/{f=1; next} f && /^version = /{gsub(/[^0-9.]/,""); print; exit}' Cargo.lock
}

changelog_version() { dpkg-parsechangelog -l debian/changelog -S Version; }

changelog_maintainer() {
  sed -n 's/^ -- \(.*<[^>]*>\)  .*/\1/p' debian/changelog | head -1
}

# Seed ENTRIES from the latest commit message when consolidating with an amend.
# The subject becomes the first bullet; body lines become further bullets, with
# git trailers and bullet markers stripped.
entries_from_commit() {
  local subject line
  subject="$(git log -1 --no-show-signature --format='%s' 2>/dev/null || true)"
  [[ -n "$subject" ]] && ENTRIES+=("$subject")
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    case "$line" in
      Co-authored-by:*|Copilot-Session:*|Signed-off-by:*|Co-developed-by:*|Reviewed-by:*|Acked-by:*|Tested-by:*|Reported-by:*) continue ;;
    esac
    line="${line#- }"; line="${line#\* }"
    ENTRIES+=("$line")
  done < <(git log -1 --no-show-signature --format='%b' 2>/dev/null || true)
}

# --- Determine current state --------------------------------------------------
CUR_CARGO="$(cargo_version)"
CUR_LOCK="$(lock_version)"
CUR_CHANGELOG="$(changelog_version)"
[[ -n "$CUR_CARGO" ]] || die "could not read version from Cargo.toml"

info "Current versions:"
info "  Cargo.toml       ${CUR_CARGO}"
info "  Cargo.lock       ${CUR_LOCK}"
info "  debian/changelog ${CUR_CHANGELOG}"
info ""

bump() {
  # bump LEVEL BASE -> prints new version
  local level="$1" base="$2"
  base="${base%%[-+]*}"
  local major minor patch tail
  major="${base%%.*}"
  tail="${base#*.}"
  minor="${tail%%.*}"
  patch="${tail##*.}"
  case "$level" in
    major) printf '%d.0.0' "$((major + 1))" ;;
    minor) printf '%d.%d.0' "$major" "$((minor + 1))" ;;
    patch) printf '%d.%d.%d' "$major" "$minor" "$((patch + 1))" ;;
  esac
}

# --- Choose the target version ------------------------------------------------
if [[ -z "$VERSION" ]]; then
  info "Choose the target version (base ${CUR_CARGO}):"
  local_reconcile=""
  if [[ "$CUR_CHANGELOG" != "$CUR_CARGO" ]]; then
    local_reconcile="$CUR_CARGO"
    info "  0) ${CUR_CARGO}   reconcile changelog to the current Cargo.toml version"
  fi
  info "  1) $(bump patch "$CUR_CARGO")   patch"
  info "  2) $(bump minor "$CUR_CARGO")   minor"
  info "  3) $(bump major "$CUR_CARGO")   major"
  info "  4) custom"
  ask choice "Selection" "${local_reconcile:+0}"
  case "$choice" in
    0) [[ -n "$local_reconcile" ]] && VERSION="$local_reconcile" || die "no reconcile option available" ;;
    1) VERSION="$(bump patch "$CUR_CARGO")" ;;
    2) VERSION="$(bump minor "$CUR_CARGO")" ;;
    3) VERSION="$(bump major "$CUR_CARGO")" ;;
    4|"") ask VERSION "New version" ;;
    *) VERSION="$choice" ;;  # allow typing a version directly
  esac
fi

semver_ok "$VERSION" || die "not a valid semantic version: ${VERSION}"
TAG="v${VERSION}"
info "Target version: ${VERSION} (tag ${TAG})"
info ""

# --- Decide commit strategy early so an amend can seed the changelog ----------
_head_subject="$(git log -1 --no-show-signature --format='%s' 2>/dev/null || true)"
if [[ -z "$COMMIT_MODE" ]]; then
  if [[ -n "$_head_subject" ]] && confirm "Amend the latest commit (\"${_head_subject}\")? Answer no to create a new commit."; then
    COMMIT_MODE="amend"
  else
    COMMIT_MODE="new"
  fi
fi

# --- Collect changelog entries ------------------------------------------------
if [[ ${#ENTRIES[@]} -eq 0 && "$COMMIT_MODE" == "amend" ]]; then
  entries_from_commit
  if [[ ${#ENTRIES[@]} -gt 0 ]]; then
    info "Changelog entries taken from the commit being amended:"
    printf '  * %s\n' "${ENTRIES[@]}" >&2
    confirm "Use these?" || ENTRIES=()
  fi
fi
if [[ ${#ENTRIES[@]} -eq 0 ]]; then
  info "Enter changelog bullet points, one per line. Blank line to finish."
  while true; do
    read -r -p "  * " line || true
    [[ -z "$line" ]] && break
    ENTRIES+=("$line")
  done
fi
[[ ${#ENTRIES[@]} -gt 0 ]] || die "at least one changelog entry is required"

MAINTAINER="$(changelog_maintainer)"
[[ -n "$MAINTAINER" ]] || MAINTAINER="$(git config user.name) <$(git config user.email)>"
DATE="$(date -R)"

changelog_stanza() {
  printf 'keylightd (%s) unstable; urgency=medium\n\n' "$VERSION"
  local line
  for line in "${ENTRIES[@]}"; do printf '  * %s\n' "$line"; done
  printf '\n -- %s  %s\n\n' "$MAINTAINER" "$DATE"
}

info "New changelog stanza:"
changelog_stanza | sed 's/^/  | /' >&2
info ""

# --- Dry run stops here after validating the generated changelog --------------
if [[ $DRY_RUN -eq 1 ]]; then
  tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
  { changelog_stanza; cat debian/changelog; } > "$tmp"
  parsed="$(dpkg-parsechangelog -l "$tmp" -S Version)"
  [[ "$parsed" == "$VERSION" ]] || die "dry run: generated changelog parses as ${parsed}, expected ${VERSION}"
  info "dry run: generated changelog validates as ${parsed}."
  info "dry run: would set Cargo.toml, Cargo.lock and debian/changelog to ${VERSION},"
  info "dry run: then ${COMMIT_MODE:-commit}, tag ${TAG}, and push. No changes made."
  exit 0
fi

# --- Apply the version everywhere ---------------------------------------------
if [[ "$CUR_CARGO" != "$VERSION" ]]; then
  sed -i "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml
  info "Updated Cargo.toml to ${VERSION}."
fi

cargo update --workspace --offline >/dev/null 2>&1 \
  || cargo update --workspace >/dev/null 2>&1 \
  || die "failed to update Cargo.lock"
info "Synced Cargo.lock."

{ changelog_stanza; cat debian/changelog; } > debian/changelog.new
mv debian/changelog.new debian/changelog
info "Prepended changelog entry."

# --- Validate the same way the release workflow does --------------------------
NEW_CARGO="$(cargo_version)"
NEW_LOCK="$(lock_version)"
NEW_CHANGELOG="$(changelog_version)"
info ""
info "Consistency check: tag=${TAG} cargo=${NEW_CARGO} lock=${NEW_LOCK} changelog=${NEW_CHANGELOG}"
[[ "$NEW_CARGO" == "$VERSION" ]] || die "Cargo.toml is ${NEW_CARGO}, expected ${VERSION}"
[[ "$NEW_LOCK" == "$VERSION" ]] || die "Cargo.lock is ${NEW_LOCK}, expected ${VERSION}"
[[ "$NEW_CHANGELOG" == "$VERSION" ]] || die "debian/changelog is ${NEW_CHANGELOG}, expected ${VERSION}"
info "Local version guard passed."
info ""

# --- Commit or amend ----------------------------------------------------------
git add Cargo.toml Cargo.lock debian/changelog
git --no-pager diff --cached --stat >&2
info ""

SIGN_FLAG=""
if [[ "$(git config --bool commit.gpgsign 2>/dev/null)" == "true" || -n "$(git config user.signingkey 2>/dev/null)" ]]; then
  SIGN_FLAG="-S"
fi

if [[ "$COMMIT_MODE" == "amend" ]]; then
  if confirm "Keep the existing commit message?"; then
    git commit $SIGN_FLAG --amend --no-edit
  else
    ask msg "New commit message" "keylightd ${VERSION}"
    git commit $SIGN_FLAG --amend -m "$msg"
  fi
else
  ask msg "Commit message" "keylightd ${VERSION}"
  git commit $SIGN_FLAG -m "$msg"
fi
info "Committed."

# --- Tag ----------------------------------------------------------------------
TAG_SIGN_FLAG="-a"
if [[ -n "$(git config user.signingkey 2>/dev/null)" ]]; then TAG_SIGN_FLAG="-s"; fi

TAG_FORCED=0
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  if confirm "Tag ${TAG} already exists. Move it to the new commit?"; then
    git tag -f $TAG_SIGN_FLAG -m "keylightd ${VERSION}" "$TAG"
    TAG_FORCED=1
  else
    die "tag ${TAG} exists and was not moved"
  fi
else
  git tag $TAG_SIGN_FLAG -m "keylightd ${VERSION}" "$TAG"
fi
info "Tagged ${TAG}."

# --- Push ---------------------------------------------------------------------
branch="$(git rev-parse --abbrev-ref HEAD)"
remote="$(git config "branch.${branch}.remote" 2>/dev/null || echo origin)"

branch_force=0
if [[ "$COMMIT_MODE" == "amend" ]] && git rev-parse -q --verify "@{u}" >/dev/null 2>&1; then
  branch_force=1
fi

# Build a single atomic push of both refs. The branch is lease-protected when it
# was rewritten by an amend; a moved tag is forced with a leading '+'.
push_opts=(--atomic)
[[ $branch_force -eq 1 ]] && push_opts+=(--force-with-lease="refs/heads/${branch}")
push_refs=("refs/heads/${branch}")
if [[ $TAG_FORCED -eq 1 ]]; then
  push_refs+=("+refs/tags/${TAG}")
else
  push_refs+=("refs/tags/${TAG}")
fi

forced_note=""
[[ $branch_force -eq 1 || $TAG_FORCED -eq 1 ]] && forced_note=" (forced)"

if [[ $DO_PUSH -eq 0 ]]; then
  info "Skipping push (--no-push). Push branch and tag together when ready:"
  info "  git push ${push_opts[*]} ${remote} ${push_refs[*]}"
  exit 0
fi

if confirm "Push branch ${branch} and tag ${TAG} to ${remote}${forced_note}?"; then
  git push "${push_opts[@]}" "$remote" "${push_refs[@]}"
  info "Pushed ${branch} and ${TAG}."
else
  info "Not pushed. Push branch and tag together when ready:"
  info "  git push ${push_opts[*]} ${remote} ${push_refs[*]}"
  exit 0
fi

info ""
info "Done. Release workflow will run for ${TAG}."
