#!/usr/bin/env bash
# Self-hosting security audit — Docker, network, SSH, headers, secrets, persistence
# Usage: bash audit-self-hosting.sh

PASS=0; WARN=0; FAIL=0
OS=$(uname -s)

ok()   { echo "[OK]   $*"; ((PASS++)); }
warn() { echo "[WARN] $*"; ((WARN++)); }
fail() { echo "[FAIL] $*"; ((FAIL++)); }
hdr()  { echo; echo "=== $* ==="; }

# ── Docker ───────────────────────────────────────────────────────────────────
hdr "Docker"

if command -v docker > /dev/null 2>&1 && docker info > /dev/null 2>&1; then
  IDS=$(docker ps -q 2>/dev/null)
  if [ -n "$IDS" ]; then
    INSPECT=$(echo "$IDS" | xargs docker inspect 2>/dev/null)

    PRIV=$(echo "$INSPECT" | jq -r '.[] | select(.HostConfig.Privileged==true) | .Name' 2>/dev/null)
    [ -n "$PRIV" ] && fail "Privileged containers: $PRIV" || ok "No privileged containers"

    ROOT_CTRS=$(echo "$INSPECT" | jq -r '.[] | select((.Config.User=="" or .Config.User=="root" or .Config.User=="0")) | .Name' 2>/dev/null)
    [ -n "$ROOT_CTRS" ] && warn "Containers running as root: $ROOT_CTRS" || ok "No root-user containers"

    SOCK=$(echo "$INSPECT" | jq -r '.[] | select(any(.Mounts[]?; .Source=="/var/run/docker.sock")) | .Name' 2>/dev/null)
    [ -n "$SOCK" ] && fail "docker.sock mounted in: $SOCK" || ok "No docker.sock mounts"

    EXPOSED=$(docker ps --format '{{.Names}}: {{.Ports}}' 2>/dev/null | grep "0\.0\.0\.0:")
    [ -n "$EXPOSED" ] && warn "Ports on all interfaces (0.0.0.0):\n$EXPOSED" || ok "No Docker ports on 0.0.0.0"
  else
    ok "No running containers"
  fi
else
  ok "Docker not running"
fi

# ── Network exposure ──────────────────────────────────────────────────────────
hdr "Network exposure (internet-facing listeners)"

if [ "$OS" = "Darwin" ]; then
  LISTENERS=$(lsof -iTCP -sTCP:LISTEN -nP 2>/dev/null | awk 'NR>1 && /\*:/ {print $9, "("$1")"}')
elif command -v ss > /dev/null 2>&1; then
  LISTENERS=$(ss -tlnp 2>/dev/null | awk 'NR>1 && /0\.0\.0\.0/ {print $4, $6}')
else
  LISTENERS=$(netstat -tlnp 2>/dev/null | awk 'NR>2 && /0\.0\.0\.0/ {print $4, $7}')
fi

if [ -n "$LISTENERS" ]; then
  UNEXPECTED=$(echo "$LISTENERS" | grep -Ev ":(22|80|443) ")
  if [ -n "$UNEXPECTED" ]; then
    warn "Unexpected internet-facing listeners:\n$UNEXPECTED"
  else
    ok "Only expected ports on all interfaces (22/80/443)"
  fi
else
  ok "No services listening on all interfaces"
fi

# ── SSH hardening ─────────────────────────────────────────────────────────────
hdr "SSH hardening"

SSHD_CONFIG="/etc/ssh/sshd_config"
if [ -f "$SSHD_CONFIG" ]; then
  grep -q "^PasswordAuthentication yes" "$SSHD_CONFIG" \
    && fail "SSH password auth enabled" || ok "SSH password auth not enabled"
  grep -q "^PermitRootLogin yes" "$SSHD_CONFIG" \
    && fail "SSH root login permitted" || ok "SSH root login not permitted"
  grep -q "^PubkeyAuthentication yes" "$SSHD_CONFIG" \
    || warn "PubkeyAuthentication not explicitly enabled in sshd_config"
else
  ok "sshd_config not found (SSH not running)"
fi

# ── Brute-force protection ────────────────────────────────────────────────────
hdr "Brute-force protection"

if command -v fail2ban-client > /dev/null 2>&1; then
  if fail2ban-client status > /dev/null 2>&1; then
    JAILS=$(fail2ban-client status 2>/dev/null | grep "Jail list" | cut -d: -f2 | tr -d ' ')
    ok "fail2ban active — jails: ${JAILS:-none}"
  else
    fail "fail2ban installed but not running"
  fi
elif [ "$OS" = "Darwin" ]; then
  PF_BLOCKS=$(pfctl -sr 2>/dev/null | grep -c "block\|drop" 2>/dev/null || echo 0)
  [ "$PF_BLOCKS" -gt 0 ] \
    && ok "pf firewall has block rules ($PF_BLOCKS)" \
    || warn "No brute-force mitigation (fail2ban not installed, pf not blocking)"
else
  warn "fail2ban not installed"
fi

# ── Reverse proxy security headers ───────────────────────────────────────────
hdr "Reverse proxy security headers"

CHECKED=0
for port in 80 443 8080 8443; do
  RESP=$(curl -sk --connect-timeout 2 --max-time 3 -I "http://localhost:$port" 2>/dev/null)
  [ -z "$RESP" ] && RESP=$(curl -sk --connect-timeout 2 --max-time 3 -I "https://localhost:$port" 2>/dev/null)
  [ -z "$RESP" ] && continue
  CHECKED=$((CHECKED+1))
  echo "$RESP" | grep -qi "x-frame-options"         || warn "Port $port: missing X-Frame-Options"
  echo "$RESP" | grep -qi "x-content-type-options"  || warn "Port $port: missing X-Content-Type-Options"
  echo "$RESP" | grep -qi "content-security-policy"  || warn "Port $port: missing Content-Security-Policy"
  echo "$RESP" | grep -qi "strict-transport-security" || warn "Port $port: missing HSTS"
  echo "$RESP" | grep -qi "^server:.*[0-9]"         && warn "Port $port: Server header exposes version"
  ok "Port $port: headers checked"
done
[ "$CHECKED" -eq 0 ] && ok "No web services found on 80/443/8080/8443"

# ── Secrets in web roots ──────────────────────────────────────────────────────
hdr "Secrets in web roots"

ENV_FOUND=0
for WEB_ROOT in /var/www /srv /opt /etc/nginx /etc/caddy "$HOME/Sites"; do
  [ -d "$WEB_ROOT" ] || continue
  ENV_FILES=$(rg --files -g ".env" -g ".env.*" -g "*.pem" -g "*.key" "$WEB_ROOT" 2>/dev/null \
    | grep -Ev "\.(example|sample|template)$")
  if [ -n "$ENV_FILES" ]; then
    fail "Sensitive files in $WEB_ROOT:\n$ENV_FILES"
    ENV_FOUND=1
  fi
done
[ "$ENV_FOUND" -eq 0 ] && ok "No .env or key files in web roots"

# ── Persistence ───────────────────────────────────────────────────────────────
hdr "Persistence"

CRON=$(crontab -l 2>/dev/null | grep -Ev "^#|^$")
[ -n "$CRON" ] && warn "Crontab entries — review:\n$CRON" || ok "Empty crontab"

if [ "$OS" = "Darwin" ]; then
  # LaunchAgents with download/exec patterns (common malware indicator)
  SUSP_LAUNCH=$(rg -l "curl|wget|bash -c|eval|base64" \
    ~/Library/LaunchAgents /Library/LaunchAgents 2>/dev/null)
  [ -n "$SUSP_LAUNCH" ] \
    && fail "LaunchAgents with download/exec patterns: $SUSP_LAUNCH" \
    || ok "No suspicious LaunchAgent patterns"
else
  # Linux: systemd user units with network fetch patterns
  SUSP_SYSTEMD=$(rg -l "curl|wget|bash -c|eval|base64" \
    ~/.config/systemd/user /etc/systemd/system 2>/dev/null)
  [ -n "$SUSP_SYSTEMD" ] \
    && fail "systemd units with download/exec patterns: $SUSP_SYSTEMD" \
    || ok "No suspicious systemd unit patterns"
fi

# ── World-writable critical paths ────────────────────────────────────────────
hdr "World-writable critical paths"

WRITABLE=$(find /etc /usr/local/bin /usr/bin -maxdepth 1 -perm -o+w 2>/dev/null)
[ -n "$WRITABLE" ] && fail "World-writable paths:\n$WRITABLE" || ok "No world-writable critical paths"

# ── Summary ──────────────────────────────────────────────────────────────────
echo
echo "─────────────────────────────────────"
echo "PASS: $PASS  WARN: $WARN  FAIL: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  echo "ACTION REQUIRED: fix FAIL items immediately"
  exit 2
elif [ "$WARN" -gt 0 ]; then
  echo "Review warnings above"
  exit 1
else
  echo "Clean"
  exit 0
fi
