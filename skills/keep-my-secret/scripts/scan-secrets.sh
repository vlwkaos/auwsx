#!/usr/bin/env bash
# Secret/credential exposure scanner
# Usage: bash scan-secrets.sh [path-to-scan]
# Exit: 0=clean, 1=warnings, 2=secrets found (rotate immediately)

set -euo pipefail

SCAN_ROOT="${1:-$(pwd)}"
PASS=0; WARN=0; FAIL=0
IS_GIT=false

ok()   { echo "[OK]   $*"; ((PASS++)) || true; }
warn() { echo "[WARN] $*"; ((WARN++)) || true; }
fail() { echo "[FAIL] $*"; ((FAIL++)) || true; }
hdr()  { echo; echo "=== $* ==="; }

if git -C "$SCAN_ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  IS_GIT=true
fi

# ── Dedicated tools ──────────────────────────────────────────────────────────
hdr "Dedicated secret scanners"

if command -v gitleaks >/dev/null 2>&1; then
  echo "[INFO] Running gitleaks..."
  if gitleaks detect --source "$SCAN_ROOT" --no-banner -q 2>/dev/null; then
    ok "gitleaks: no secrets found"
  else
    fail "gitleaks: secrets detected — review output above"
  fi
else
  warn "gitleaks not installed (brew install gitleaks recommended)"
fi

if command -v trufflehog >/dev/null 2>&1; then
  echo "[INFO] Running trufflehog..."
  if trufflehog filesystem "$SCAN_ROOT" --no-update --json 2>/dev/null | grep -q '"Verified":true'; then
    fail "trufflehog: verified secrets detected"
  else
    ok "trufflehog: no verified secrets"
  fi
else
  warn "trufflehog not installed (brew install trufflehog recommended)"
fi

# ── Regex patterns (rg fallback) ─────────────────────────────────────────────
hdr "Pattern scan (rg)"

RG_OPTS=(--no-heading -n -i
  --glob '!*.lock' --glob '!*.sum' --glob '!*.png' --glob '!*.jpg'
  --glob '!node_modules' --glob '!.git' --glob '!vendor'
)

scan_pattern() {
  local label="$1"; local pattern="$2"
  local hits
  hits=$(rg "${RG_OPTS[@]}" "$pattern" "$SCAN_ROOT" 2>/dev/null || true)
  if [ -n "$hits" ]; then
    fail "$label"
    echo "$hits" | head -20
  else
    ok "$label"
  fi
}

# High-confidence patterns
scan_pattern "AWS access key"          'AKIA[0-9A-Z]{16}'
scan_pattern "AWS secret key literal"  '(?i)aws.{0,20}secret.{0,20}[=:]\s*["\x27][A-Za-z0-9/+]{40}'
scan_pattern "Private key header"      '-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY'
scan_pattern "GitHub PAT (ghp_)"       'ghp_[A-Za-z0-9]{36}'
scan_pattern "GitHub PAT (github_pat)" 'github_pat_[A-Za-z0-9_]{82}'
scan_pattern "Slack token"             'xox[baprs]-[0-9A-Za-z\-]{10,}'
scan_pattern "Stripe live key"         'sk_live_[0-9a-zA-Z]{24,}'
scan_pattern "Stripe test key literal" 'sk_test_[0-9a-zA-Z]{24,}'
scan_pattern "Twilio account SID"      'AC[a-fA-F0-9]{32}'
scan_pattern "SendGrid key"            'SG\.[A-Za-z0-9\-_]{22}\.[A-Za-z0-9\-_]{43}'
scan_pattern "Password in DB URL"      '(postgres|mysql|mongodb|redis)://[^:@\s]+:[^@\s]{6,}@'
scan_pattern "Bearer token hardcoded"  '(?i)(Authorization|Bearer)["\x27]?\s*[:=]\s*["\x27][A-Za-z0-9\-_.~+/]{20,}'
scan_pattern "Generic secret assign"   '(?i)(password|passwd|secret|api_key|apikey|token)\s*=\s*["\x27][^"\x27\s]{8,}["\x27]'

# ── Git-tracked sensitive files ───────────────────────────────────────────────
hdr "Git-tracked sensitive files"

if [ "$IS_GIT" = true ]; then
  TRACKED_SECRETS=$(git -C "$SCAN_ROOT" ls-files 2>/dev/null \
    | grep -E '\.(env|pem|key|p12|pfx|jks|keystore|pkcs12)$|id_rsa|id_dsa|id_ecdsa|id_ed25519' || true)
  if [ -n "$TRACKED_SECRETS" ]; then
    fail "Sensitive files tracked by git:"
    echo "$TRACKED_SECRETS"
  else
    ok "No sensitive file types tracked by git"
  fi

  # .env files present but maybe gitignored
  ENV_FILES=$(find "$SCAN_ROOT" -maxdepth 4 -name '*.env' -o -name '.env' -o -name '.env.*' 2>/dev/null \
    | grep -v '.git' || true)
  IGNORED_OK=true
  for f in $ENV_FILES; do
    if git -C "$SCAN_ROOT" check-ignore -q "$f" 2>/dev/null; then
      ok ".env gitignored: $f"
    else
      fail ".env NOT gitignored: $f"
      IGNORED_OK=false
    fi
  done
  [ -z "$ENV_FILES" ] && ok "No .env files found"

  # Check git history for ever-committed secrets (last 500 commits, capped)
  hdr "Git history — ever-committed .env / key files"
  HISTORY_HITS=$(git -C "$SCAN_ROOT" log --all --diff-filter=A --name-only \
    --pretty=format: --max-count=500 2>/dev/null \
    | grep -E '\.(env|pem|key|p12|pfx|id_rsa|id_dsa|id_ecdsa|id_ed25519)$' || true)
  if [ -n "$HISTORY_HITS" ]; then
    fail "Sensitive files found in git history (even if deleted now):"
    echo "$HISTORY_HITS"
    echo "  -> Use BFG or git-filter-repo to purge"
  else
    ok "No sensitive files found in git history"
  fi
else
  warn "Not a git repo — skipping git checks"
fi

# ── .gitignore hygiene ───────────────────────────────────────────────────────
hdr ".gitignore hygiene"

if [ "$IS_GIT" = true ] && [ -f "$SCAN_ROOT/.gitignore" ]; then
  for pat in '.env' '*.pem' '*.key' '*.p12' 'secrets/' '.secrets'; do
    if grep -qF "$pat" "$SCAN_ROOT/.gitignore" 2>/dev/null; then
      ok ".gitignore covers: $pat"
    else
      warn ".gitignore missing: $pat"
    fi
  done
else
  [ "$IS_GIT" = true ] && warn "No .gitignore found at repo root"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo
echo "────────────────────────────────────────"
printf "PASS: %d  WARN: %d  FAIL: %d\n" "$PASS" "$WARN" "$FAIL"
echo "────────────────────────────────────────"

if   [ "$FAIL" -gt 0 ]; then exit 2
elif [ "$WARN" -gt 0 ]; then exit 1
else exit 0
fi
