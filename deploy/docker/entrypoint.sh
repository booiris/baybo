#!/bin/sh
set -eu

die() {
    printf 'baybo-entrypoint: %s\n' "$*" >&2
    exit 1
}

require_absolute_path() {
    case "$2" in
        /*) ;;
        *) die "$1 must be an absolute path (got: $2)" ;;
    esac
}

normalize_boolean() {
    case "$2" in
        1|true|TRUE|yes|YES) printf 'true' ;;
        0|false|FALSE|no|NO) printf 'false' ;;
        *) die "$1 must be true or false (got: $2)" ;;
    esac
}

workspace=${BAYBO_WORKSPACE:-/var/lib/baybo}
config_file=${BAYBO_CONFIG_PATH:-${workspace}/config/baybo.json}
key_file=${BAYBO_ENCRYPTION_KEY_FILE:-${workspace}/.key/encryption.key}
gateway_port=${BAYBO_GATEWAY_PORT:-8888}

require_absolute_path BAYBO_WORKSPACE "$workspace"
require_absolute_path BAYBO_CONFIG_PATH "$config_file"
require_absolute_path BAYBO_ENCRYPTION_KEY_FILE "$key_file"

case "$gateway_port" in
    *[!0-9]*|'') die "BAYBO_GATEWAY_PORT must be an integer" ;;
esac
if [ "$gateway_port" -lt 1 ] || [ "$gateway_port" -gt 65535 ]; then
    die "BAYBO_GATEWAY_PORT must be between 1 and 65535"
fi

browser_enable=$(normalize_boolean BAYBO_BROWSER_ENABLE "${BAYBO_BROWSER_ENABLE:-false}")
permission=${BAYBO_PERMISSION:-auto}
case "$permission" in
    auto|manual|free) ;;
    *) die "BAYBO_PERMISSION must be auto, manual, or free" ;;
esac

mkdir -p \
    "$workspace" \
    "$workspace/home" \
    "$(dirname "$config_file")" \
    "$(dirname "$key_file")" \
    "${XDG_CACHE_HOME:-/var/cache/baybo}"

if [ ! -f "$key_file" ]; then
    key_tmp="${key_file}.tmp.$$"
    umask 077
    openssl rand -hex 32 >"$key_tmp"
    chmod 0600 "$key_tmp"
    mv "$key_tmp" "$key_file"
    printf 'baybo-entrypoint: generated encryption key at %s\n' "$key_file"
fi

if [ ! -f "$config_file" ]; then
    llm_api_key=${BAYBO_LLM_API_KEY:-}
    llm_name=${BAYBO_LLM_NAME:-primary}
    llm_provider=${BAYBO_LLM_PROVIDER:-deepseek}
    llm_model=${BAYBO_LLM_MODEL:-deepseek-chat}
    llm_base_url=${BAYBO_LLM_BASE_URL:-}
    reasoning_effort=${BAYBO_REASONING_EFFORT:-medium}

    [ -n "$llm_api_key" ] || die "BAYBO_LLM_API_KEY is required on first boot"
    [ -n "$llm_name" ] || die "BAYBO_LLM_NAME must not be empty"
    [ -n "$llm_provider" ] || die "BAYBO_LLM_PROVIDER must not be empty"
    [ -n "$llm_model" ] || die "BAYBO_LLM_MODEL must not be empty"
    [ -n "$reasoning_effort" ] || die "BAYBO_REASONING_EFFORT must not be empty"

    config_tmp="${config_file}.tmp.$$"
    umask 077
    jq -n \
        --arg name "$llm_name" \
        --arg provider "$llm_provider" \
        --arg model "$llm_model" \
        --arg base_url "$llm_base_url" \
        --arg reasoning_effort "$reasoning_effort" \
        --arg workspace "$workspace" \
        --arg key_file "$key_file" \
        --arg permission "$permission" \
        --argjson gateway_port "$gateway_port" \
        --argjson browser_enable "$browser_enable" \
        '
        {
          llm: [
            {
              name: $name,
              provider: $provider,
              model: $model,
              api_key_env: "BAYBO_LLM_API_KEY",
              reasoning_effort: $reasoning_effort
            }
          ],
          "default-llm": $name,
          workspace: {
            path: $workspace
          },
          security: {
            encryption_key_file: $key_file,
            leak_detection_enabled: true
          },
          gateway: {
            enabled: true,
            bind_address: "0.0.0.0",
            port: $gateway_port,
            cors_allowed_origins: [],
            shutdown_grace_secs: 30
          },
          browser: {
            enable: $browser_enable,
            sandbox: false,
            docker: {
              enable: false
            }
          },
          permission: $permission
        }
        | if $base_url == "" then . else .llm[0].base_url = $base_url end
        ' >"$config_tmp"
    chmod 0600 "$config_tmp"
    mv "$config_tmp" "$config_file"
    printf 'baybo-entrypoint: generated initial config at %s\n' "$config_file"
fi

exec "$@"
