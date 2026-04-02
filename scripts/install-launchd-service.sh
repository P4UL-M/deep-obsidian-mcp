#!/bin/zsh
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname "$0")/.." && pwd)"
VAULT_PATH="${1:-${OBSIDIAN_VAULT_PATH:-}}"
CONFIG_PATH="${DEEP_OBSIDIAN_CONFIG_PATH:-}"
SERVER_BIN="${DEEP_OBSIDIAN_SERVER_BIN:-${ROOT_DIR}/bin/deep-obsidian-mcp}"
if [[ "${SERVER_BIN}" == "${ROOT_DIR}/bin/deep-obsidian-mcp" ]]; then
  for candidate in \
    "${ROOT_DIR}/target/release/deep-obsidian-mcp" \
    "${ROOT_DIR}/target/debug/deep-obsidian-mcp"; do
    if [[ -x "${candidate}" ]]; then
      SERVER_BIN="${candidate}"
      break
    fi
  done
fi

LABEL="${DEEP_OBSIDIAN_LABEL:-io.deep-obsidian-mcp}"
HOST="${DEEP_OBSIDIAN_HOST:-127.0.0.1}"
PORT="${DEEP_OBSIDIAN_PORT:-4100}"
MCP_PATH="${DEEP_OBSIDIAN_MCP_PATH:-/mcp}"
HEALTH_PATH="${DEEP_OBSIDIAN_HEALTH_PATH:-/healthz}"
EMBEDDING_PROVIDER_VALUE="${DEEP_OBSIDIAN_EMBEDDING_PROVIDER:-${EMBEDDING_PROVIDER:-}}"
EMBEDDING_MODEL_VALUE="${DEEP_OBSIDIAN_EMBEDDING_MODEL:-${EMBEDDING_MODEL:-${OPENAI_EMBEDDING_MODEL:-}}}"
EMBEDDING_BASE_URL_VALUE="${DEEP_OBSIDIAN_EMBEDDING_BASE_URL:-${EMBEDDING_BASE_URL:-${OPENAI_BASE_URL:-}}}"
EMBEDDING_API_KEY_VALUE="${DEEP_OBSIDIAN_EMBEDDING_API_KEY:-${EMBEDDING_API_KEY:-${OPENAI_API_KEY:-}}}"
LAUNCH_AGENTS_DIR="${HOME}/Library/LaunchAgents"
LOG_DIR="${HOME}/Library/Logs/${LABEL}"
PLIST_PATH="${LAUNCH_AGENTS_DIR}/${LABEL}.plist"

if [[ -z "${EMBEDDING_PROVIDER_VALUE}" && -n "${EMBEDDING_MODEL_VALUE}" ]]; then
  EMBEDDING_PROVIDER_VALUE="openai-compatible"
fi

function xml_escape() {
  local value="$1"
  value="${value//&/&amp;}"
  value="${value//</&lt;}"
  value="${value//>/&gt;}"
  value="${value//\"/&quot;}"
  value="${value//\'/&apos;}"
  printf '%s' "${value}"
}

typeset -a env_entries
typeset -a path_entries
path_entries=()
for path_dir in /opt/homebrew/bin /usr/local/bin /usr/bin /bin /usr/sbin /sbin; do
  if [[ -d "${path_dir}" ]]; then
    path_entries+=("${path_dir}")
  fi
done
PATH_VALUE="${(j/:/)path_entries}"
env_entries=(
  "    <key>PATH</key>"
  "    <string>${PATH_VALUE}</string>"
)

function add_env_entry() {
  local key="$1"
  local value="$2"
  if [[ -n "${value}" ]]; then
    env_entries+=("    <key>${key}</key>")
    env_entries+=("    <string>$(xml_escape "${value}")</string>")
  fi
}

add_env_entry "EMBEDDING_PROVIDER" "${EMBEDDING_PROVIDER_VALUE}"
add_env_entry "EMBEDDING_MODEL" "${EMBEDDING_MODEL_VALUE}"
add_env_entry "EMBEDDING_BASE_URL" "${EMBEDDING_BASE_URL_VALUE}"
add_env_entry "EMBEDDING_API_KEY" "${EMBEDDING_API_KEY_VALUE}"
ENV_BLOCK="$(printf '%s\n' "${env_entries[@]}")"

mkdir -p "${LAUNCH_AGENTS_DIR}" "${LOG_DIR}"

typeset -a program_arguments
program_arguments=(
  "    <string>${SERVER_BIN}</string>"
  "    <string>serve</string>"
  "    <string>--transport</string>"
  "    <string>http</string>"
  "    <string>--host</string>"
  "    <string>${HOST}</string>"
  "    <string>--port</string>"
  "    <string>${PORT}</string>"
  "    <string>--mcp-path</string>"
  "    <string>${MCP_PATH}</string>"
  "    <string>--health-path</string>"
  "    <string>${HEALTH_PATH}</string>"
)

if [[ -n "${CONFIG_PATH}" ]]; then
  program_arguments+=("    <string>--config</string>")
  program_arguments+=("    <string>${CONFIG_PATH}</string>")
fi
if [[ -n "${VAULT_PATH}" ]]; then
  program_arguments+=("    <string>--vault</string>")
  program_arguments+=("    <string>${VAULT_PATH}</string>")
fi

PROGRAM_ARGUMENTS_BLOCK="$(printf '%s\n' "${program_arguments[@]}")"

cat > "${PLIST_PATH}" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
${PROGRAM_ARGUMENTS_BLOCK}
  </array>
  <key>WorkingDirectory</key>
  <string>${ROOT_DIR}</string>
  <key>EnvironmentVariables</key>
  <dict>
${ENV_BLOCK}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${LOG_DIR}/stdout.log</string>
  <key>StandardErrorPath</key>
  <string>${LOG_DIR}/stderr.log</string>
</dict>
</plist>
PLIST

launchctl bootout "gui/$(id -u)" "${PLIST_PATH}" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "${PLIST_PATH}"
launchctl kickstart -k "gui/$(id -u)/${LABEL}"

echo "Installed ${LABEL}"
echo "plist: ${PLIST_PATH}"
if [[ -n "${CONFIG_PATH}" ]]; then
  echo "config: ${CONFIG_PATH}"
elif [[ -n "${VAULT_PATH}" ]]; then
  echo "vault override: ${VAULT_PATH}"
else
  echo "config: default deep-obsidian-mcp config path"
fi
echo "mcp endpoint: http://${HOST}:${PORT}${MCP_PATH}"
echo "health endpoint: http://${HOST}:${PORT}${HEALTH_PATH}"
if [[ -n "${EMBEDDING_PROVIDER_VALUE}" && -n "${EMBEDDING_MODEL_VALUE}" ]]; then
  echo "embedding mode: ${EMBEDDING_PROVIDER_VALUE} / ${EMBEDDING_MODEL_VALUE}"
else
  echo "embedding mode: disabled (set EMBEDDING_MODEL + EMBEDDING_API_KEY or OPENAI_EMBEDDING_MODEL + OPENAI_API_KEY before reinstalling)"
fi
