#!/usr/bin/env bash
# Supply chain IOC audit — see supply-chain-cve-index.md
# Usage: bash audit-supply-chain.sh [path-to-scan]

SCAN_ROOT="${1:-$HOME}"
PASS=0; WARN=0; FAIL=0

ok()   { echo "[OK]   $*"; ((PASS++)); }
warn() { echo "[WARN] $*"; ((WARN++)); }
fail() { echo "[FAIL] $*"; ((FAIL++)); }
hdr()  { echo; echo "=== $* ==="; }

# ── Python: litellm (CVE: TeamPCP, Mar 2026) ────────────────────────────────
hdr "litellm 1.82.7/1.82.8 (PyPI, Mar 2026)"

PTH_HITS=$(rg --files -g "litellm_init.pth" "$SCAN_ROOT" 2>/dev/null)
if [ -n "$PTH_HITS" ]; then
  fail "litellm_init.pth found — COMPROMISED"
  echo "$PTH_HITS"
else
  ok "litellm_init.pth not found"
fi

if [ -f "$HOME/.config/sysmon/sysmon.py" ]; then
  fail "Persistence backdoor: ~/.config/sysmon/sysmon.py"
else
  ok "No sysmon persistence"
fi
if [ -f "$HOME/.config/systemd/user/sysmon.service" ]; then
  fail "Persistence service: ~/.config/systemd/user/sysmon.service"
else
  ok "No sysmon.service"
fi

for cmd in pip pip3; do
  VER=$(command -v $cmd > /dev/null && $cmd show litellm 2>/dev/null | grep ^Version | awk '{print $2}')
  if [ -n "$VER" ]; then
    if [[ "$VER" == "1.82.7" || "$VER" == "1.82.8" ]]; then
      fail "litellm $VER installed via $cmd — MALICIOUS VERSION"
    else
      warn "litellm $VER installed via $cmd (check if in affected range)"
    fi
  fi
done

UV_HIT=$(uv tool list 2>/dev/null | grep litellm)
[ -n "$UV_HIT" ] && warn "litellm in uv tools: $UV_HIT" || ok "litellm not in uv tools"
PX_HIT=$(pipx list 2>/dev/null | grep litellm)
[ -n "$PX_HIT" ] && warn "litellm in pipx: $PX_HIT" || ok "litellm not in pipx"

for cmd in pip pip3; do
  DSPY=$(command -v $cmd > /dev/null && $cmd show dspy dspy-ai 2>/dev/null | grep ^Version)
  [ -n "$DSPY" ] && warn "dspy installed ($DSPY) — may have pulled litellm transitively"
done

LITELLM_DEPS=$(rg -l "litellm" -g "requirements*.txt" -g "pyproject.toml" -g "*.lock" "$SCAN_ROOT" 2>/dev/null)
[ -n "$LITELLM_DEPS" ] && warn "litellm in dep files:\n$LITELLM_DEPS" || ok "litellm not in ws dep files"

# ── Python: ultralytics (Dec 2024) ──────────────────────────────────────────
hdr "ultralytics 8.3.41-8.3.46 (PyPI, Dec 2024)"

for cmd in pip pip3; do
  VER=$(command -v $cmd > /dev/null && $cmd show ultralytics 2>/dev/null | grep ^Version | awk '{print $2}')
  if [ -n "$VER" ]; then
    case "$VER" in
      8.3.41|8.3.42|8.3.45|8.3.46)
        fail "ultralytics $VER — MALICIOUS VERSION (cryptominer)" ;;
      *)
        warn "ultralytics $VER installed — not in known-bad list" ;;
    esac
  fi
done

if pgrep -x xmrig > /dev/null 2>&1; then
  fail "xmrig process running — possible cryptominer"
else
  ok "No xmrig process"
fi

# ── Python: SilentSync RAT (Jul–Aug 2025) ───────────────────────────────────
hdr "SilentSync RAT — termncolor/sisaws/secmeasure (PyPI, Jul–Aug 2025)"

for pkg in termncolor sisaws secmeasure colorinal; do
  for cmd in pip pip3; do
    VER=$(command -v $cmd > /dev/null && $cmd show "$pkg" 2>/dev/null | grep ^Version | awk '{print $2}')
    [ -n "$VER" ] && fail "$pkg $VER installed via $cmd — MALICIOUS"
  done
done

LAUNCHD_IOC=$(rg -l "200\.58\.107\.25" ~/Library/LaunchAgents 2>/dev/null)
[ -n "$LAUNCHD_IOC" ] && fail "SilentSync C2 in LaunchAgent: $LAUNCHD_IOC" || ok "No SilentSync C2 in LaunchAgents"

# ── npm: @dydxprotocol/v4-client-js (Jan–Feb 2026) ──────────────────────────
hdr "@dydxprotocol/v4-client-js malicious versions (npm, Jan–Feb 2026)"

for pkg in "@dydxprotocol/v4-client-js" "dydx-v4-client"; do
  VER=$(npm list "$pkg" 2>/dev/null | grep -o "@[0-9][0-9.]*" | tr -d '@' | head -1)
  if [ -n "$VER" ]; then
    if [[ "$VER" =~ ^(3\.4\.1|1\.22\.1|1\.15\.2|1\.0\.31)$ ]]; then
      fail "$pkg $VER — MALICIOUS VERSION (wallet stealer)"
    else
      warn "$pkg $VER installed — verify not in 3.4.1/1.22.1/1.15.2/1.0.31"
    fi
  fi
done

if command -v lsof > /dev/null 2>&1; then
  DYDX_C2=$(lsof -i 2>/dev/null | grep "dydx.priceoracle.site")
  [ -n "$DYDX_C2" ] && fail "Active connection to dydx C2: $DYDX_C2" || ok "No dydx C2 connections"
fi

# ── npm: nx (CVE-2025-10894) ─────────────────────────────────────────────────
hdr "nx CVE-2025-10894 (npm, Aug 2025)"

if command -v gh > /dev/null && command -v jq > /dev/null 2>&1; then
  S1NG=$(gh api /user/repos --paginate 2>/dev/null | jq -r '.[].name' 2>/dev/null | grep "^s1ngularity-repository-")
  [ -n "$S1NG" ] && fail "s1ngularity repo in GitHub account: $S1NG" || ok "No s1ngularity repos in GitHub"
else
  warn "gh+jq not available — manually verify no s1ngularity-repository-* repos in your GitHub account"
fi

NX_TELEMETRY=$(rg --files -g "telemetry.js" "$SCAN_ROOT" 2>/dev/null | grep "/nx/")
[ -n "$NX_TELEMETRY" ] && fail "Suspicious telemetry.js in nx package: $NX_TELEMETRY" || ok "No nx telemetry.js"

# ── npm: Shai-Hulud 2.0 (Sep-Nov 2025) ──────────────────────────────────────
hdr "Shai-Hulud 2.0 (npm worm, Sep-Nov 2025)"

SH_HITS=$({ rg --files -g "setup_bun.js" "$SCAN_ROOT" 2>/dev/null; rg --files -g "bun_environment.js" "$SCAN_ROOT" 2>/dev/null; } | grep -v "node_modules/.cache")
[ -n "$SH_HITS" ] && fail "Shai-Hulud files found:\n$SH_HITS" || ok "No Shai-Hulud files"

RUNNER_FILES=$(rg --files -g ".runner" "$HOME" --max-depth 4 2>/dev/null)
if [ -n "$RUNNER_FILES" ]; then
  if echo "$RUNNER_FILES" | xargs grep -q "SHA1HULUD" 2>/dev/null; then
    fail "SHA1HULUD runner config found"
  else
    warn "GitHub Actions runner installed — verify runner name is not SHA1HULUD"
  fi
else
  ok "No self-hosted runner config found"
fi

# ── Active C2 connections ────────────────────────────────────────────────────
hdr "Active C2 connections (known IOC domains)"

C2_DOMAINS=(
  "models.litellm.cloud"
  "checkmarx.zone"
  "webhook.site"
  "dydx.priceoracle.site"
  "200.58.107.25"
)

if command -v lsof > /dev/null 2>&1; then
  CONNS=$(lsof -i -nP 2>/dev/null)
  for domain in "${C2_DOMAINS[@]}"; do
    HIT=$(echo "$CONNS" | grep "$domain")
    [ -n "$HIT" ] && fail "Active connection to C2 $domain: $HIT" || ok "No connection to $domain"
  done
else
  warn "lsof not available — cannot check active C2 connections"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo
echo "─────────────────────────────────────"
echo "PASS: $PASS  WARN: $WARN  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  echo "ACTION REQUIRED: rotate all credentials immediately"
  exit 2
elif [ "$WARN" -gt 0 ]; then
  echo "Review warnings above"
  exit 1
else
  echo "Clean"
  exit 0
fi
