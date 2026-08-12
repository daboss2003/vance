#!/usr/bin/env bash
# Dev helper: publish / revoke / list marketplace effects on a LOCAL running
# accounts service, so the M2 pipeline (My Effects → Install → gate) can be
# tested end-to-end before the catalog (M3) and payments (M4) exist.
#
# It signs in as the built-in `dev` provider's default user — the SAME user the
# app's "Local dev sign-in" uses — so anything published here shows up in the
# app's Settings → My Effects.
#
#   ./dev-effects.sh publish <path-to.wasm> <sku> [display name]   # paid (M2 gate)
#   ./dev-effects.sh catalog <path-to.wasm> <sku> <publisher> <name> [desc] [category]
#   ./dev-effects.sh revoke  <sku>
#   ./dev-effects.sh list
#
# Requires: curl, python3. Base URL from $VANCE_ACCOUNTS_PUBLIC_URL (default
# http://127.0.0.1:8787). The dev provider must be enabled (it is by default).
set -euo pipefail

BASE="${VANCE_ACCOUNTS_PUBLIC_URL:-http://127.0.0.1:8787}"
VERIFIER="dev-effects-verifier-0123456789-abcdefghijklmnopqrstuvwxyz"

jget() { python3 -c "import sys,json;print(json.load(sys.stdin).get('$1',''))"; }

sign_in() {
  local challenge
  challenge=$(python3 -c "import hashlib,base64;print(base64.urlsafe_b64encode(hashlib.sha256(b'$VERIFIER').digest()).rstrip(b'=').decode())")
  local loc code
  loc=$(curl -s -o /dev/null -w '%{redirect_url}' \
    "$BASE/auth/start?provider=dev&code_challenge=$challenge&redirect_uri=http://127.0.0.1:9999/cb&device_id=dev-cli&device_name=dev-cli&platform=cli")
  code=$(printf '%s' "$loc" | sed -n 's/.*[?&]code=\([^&]*\).*/\1/p')
  [ -n "$code" ] || { echo "sign-in failed (is the server running at $BASE?)" >&2; exit 1; }
  curl -s -X POST "$BASE/auth/token" -H 'content-type: application/json' \
    -d "{\"code\":\"$code\",\"code_verifier\":\"$VERIFIER\"}" | jget access_token
}

cmd="${1:-}"; shift || true
ACCESS=$(sign_in)

case "$cmd" in
  publish)
    WASM="${1:?path to .wasm required}"; SKU="${2:?sku required}"; NAME="${3:-$SKU}"
    ABS=$(python3 -c "import os,sys;print(os.path.abspath(sys.argv[1]))" "$WASM")
    [ -f "$ABS" ] || { echo "no such file: $ABS" >&2; exit 1; }
    DEST="$(basename "$ABS")"
    BODY=$(python3 - "$SKU" "$ABS" "$DEST" "$NAME" <<'PY'
import json, sys
print(json.dumps({"sku": sys.argv[1], "file_path": sys.argv[2],
                  "dest": sys.argv[3], "display_name": sys.argv[4]}))
PY
)
    curl -s -X POST "$BASE/v1/dev/publish" -H "authorization: Bearer $ACCESS" \
      -H 'content-type: application/json' -d "$BODY"
    echo
    echo "Published '$SKU' and granted it to the dev user. Open the app → Settings → My Effects."
    ;;
  catalog)
    WASM="${1:?path to .wasm required}"; SKU="${2:?sku required}"
    PUBLISHER="${3:?publisher required}"; NAME="${4:?display name required}"
    DESC="${5:-}"; CATEGORY="${6:-}"
    ABS=$(python3 -c "import os,sys;print(os.path.abspath(sys.argv[1]))" "$WASM")
    [ -f "$ABS" ] || { echo "no such file: $ABS" >&2; exit 1; }
    DEST="$(basename "$ABS")"
    BODY=$(python3 - "$SKU" "$ABS" "$DEST" "$PUBLISHER" "$NAME" "$DESC" "$CATEGORY" <<'PY'
import json, sys
sku, path, dest, publisher, name, desc, category = sys.argv[1:8]
print(json.dumps({
    "sku": sku, "file_path": path, "dest": dest, "publisher": publisher,
    "display_name": name, "description": desc or None, "kind": "effect",
    "categories": [category] if category else [], "tags": [], "capabilities": []
}))
PY
)
    curl -s -X POST "$BASE/v1/dev/catalog-publish" -H 'content-type: application/json' -d "$BODY"
    echo
    echo "Listed '$SKU' (free) in the catalog. Open the app → add-on effects → Browse."
    ;;
  revoke)
    SKU="${1:?sku required}"
    curl -s -X POST "$BASE/v1/dev/revoke" -H "authorization: Bearer $ACCESS" \
      -H 'content-type: application/json' -d "{\"sku\":\"$SKU\"}"
    echo
    echo "Revoked '$SKU'. The app should uninstall it and the gate should block it."
    ;;
  list)
    curl -s "$BASE/v1/entitlements" -H "authorization: Bearer $ACCESS"; echo
    ;;
  *)
    echo "usage: $0 {publish <wasm> <sku> [name] | catalog <wasm> <sku> <publisher> <name> [desc] [category] | revoke <sku> | list}" >&2
    exit 2
    ;;
esac
