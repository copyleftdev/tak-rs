#!/usr/bin/env bash
# Deploy the marketing site at assets/site/ to the gh-pages branch.
#
# The source tree keeps the site under assets/site/ with brand assets
# at the sibling assets/brand/. GitHub Pages serves a single root, so
# this script stages a self-contained copy: the site contents go to
# the deploy root, the brand folder is copied in as ./brand/, and the
# ../brand/ references in index.html + site.webmanifest are rewritten
# to brand/ so the deployed page stays self-contained.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

ORIGIN_URL="$(git remote get-url origin)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp -a assets/site/. "$STAGE/"
cp -a assets/brand/. "$STAGE/brand/"

# Rewrite ../brand/ -> brand/ in the deployed copies only (the source
# stays as-is so the local python http.server flow at the repo root
# keeps working unchanged).
sed -i 's|\.\./brand/|brand/|g' "$STAGE/index.html" "$STAGE/site.webmanifest"

# .nojekyll disables Jekyll processing so .well-known/ and underscore-
# prefixed paths are served verbatim.
touch "$STAGE/.nojekyll"

cd "$STAGE"
git init -q -b gh-pages
git add -A
git -c user.name='tak-rs-pages' \
    -c user.email='dj@codetestcode.io' \
    commit -qm "deploy: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
git remote add origin "$ORIGIN_URL"
git push --force --quiet origin gh-pages

echo "deployed → https://copyleftdev.github.io/tak-rs/"
