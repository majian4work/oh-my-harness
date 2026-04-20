#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
DEVCONTAINER_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
WORKSPACE_DIR=$(CDPATH= cd -- "$DEVCONTAINER_DIR/.." && pwd)
TEMPLATE_FILE="$DEVCONTAINER_DIR/docker-compose.template.yml"
COMPOSE_FILE="$WORKSPACE_DIR/.devcontainer.compose.yml"
RUNTIME_ENV_FILE="$WORKSPACE_DIR/.devcontainer.runtime.env"
CONFIG_FILE="$WORKSPACE_DIR/.devcontainer.ini"
DEFAULT_NO_PROXY="localhost,127.0.0.1/8,10.0.0.0/8"

# shellcheck disable=SC1090
source "$SCRIPT_DIR/helper.sh"

escape_sed_replacement() {
    printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
}

validate_env_name() {
    var_name=$1

    case "$var_name" in
        ''|[0-9]*|*[!A-Za-z0-9_]*)
            return 1
            ;;
    esac

    return 0
}

append_runtime_entry() {
    var_name=$1
    var_value=$2

    if ! validate_env_name "$var_name"; then
        echo "Skipping invalid runtime env name: $var_name" >&2
        return 0
    fi

    printf '%s=%s\n' "$var_name" "$var_value" >> "$RUNTIME_ENV_FILE"
}

append_runtime_config_entry() {
    var_name=$1
    var_value=$2

    case "$var_name" in
        HF_HOME|CCACHE_DIR|DEVCONTAINER_INIT_INCLUDE|DISPLAY|http_proxy|https_proxy|no_proxy)
            echo "Skipping reserved runtime env name from [env] $var_name" >&2
            return 0
            ;;
    esac

    append_runtime_entry "$var_name" "$var_value"
}

append_runtime_entry_if_set() {
    var_name=$1
    var_value=${2:-}

    if [ -n "$var_value" ]; then
        append_runtime_entry "$var_name" "$var_value"
    fi
}

resolve_group_id() {
    group_name=$1
    device_glob=$2
    group_id=$(getent group "$group_name" | cut -d: -f3 || true)
    if [ -n "$group_id" ]; then
        printf '%s\n' "$group_id"
        return 0
    fi

    set -- $device_glob
    if [ "$1" != "$device_glob" ] && [ -e "$1" ]; then
        stat -c '%g' "$1"
        return 0
    fi

    printf '%s\n' "$GROUPID"
}

find_nearest_existing_parent() {
    path=$1

    while [ ! -e "$path" ]; do
        parent=$(dirname "$path")
        if [ "$parent" = "$path" ]; then
            break
        fi
        path=$parent
    done

    printf '%s\n' "$path"
}

ensure_dir_with_optional_sudo() {
    target_dir=$1

    if [ -d "$target_dir" ]; then
        return 0
    fi

    parent_dir=$(find_nearest_existing_parent "$target_dir")
    if [ -w "$parent_dir" ] && [ -x "$parent_dir" ]; then
        mkdir -p "$target_dir"
        return 0
    fi

    echo "Current user $(id -un) cannot create $target_dir under $parent_dir; retrying with sudo." >&2
    sudo mkdir -p "$target_dir"
}

dir_has_required_access() {
    target_dir=$1

    [ -d "$target_dir" ] && [ -r "$target_dir" ] && [ -w "$target_dir" ] && [ -x "$target_dir" ]
}

setfacl_with_optional_sudo() {
    target_path=$1
    acl_spec=$2
    default_acl=${3:-false}
    owner_uid=$(stat -c '%u' "$target_path")

    if [ "$default_acl" = true ]; then
        if [ "$owner_uid" -eq "$USERID" ]; then
            setfacl -d -m "$acl_spec" "$target_path"
            return 0
        fi

        echo "Current user $(id -un) does not own $target_path; applying default ACL with sudo." >&2
        sudo setfacl -d -m "$acl_spec" "$target_path"
        return 0
    fi

    if [ "$owner_uid" -eq "$USERID" ]; then
        setfacl -m "$acl_spec" "$target_path"
        return 0
    fi

    echo "Current user $(id -un) does not own $target_path; applying ACL with sudo." >&2
    sudo setfacl -m "$acl_spec" "$target_path"
}

ensure_mount_dir_access() {
    target_dir=$1

    ensure_dir_with_optional_sudo "$target_dir"

    if dir_has_required_access "$target_dir"; then
        return 0
    fi

    if ! command -v setfacl >/dev/null 2>&1; then
        echo "Current user $(id -un) lacks rwx access to $target_dir and setfacl is unavailable." >&2
        return 1
    fi

    echo "Current user $(id -un) lacks rwx access to $target_dir; applying ACL once." >&2
    setfacl_with_optional_sudo "$target_dir" "u:${USERID}:rwx"
    setfacl_with_optional_sudo "$target_dir" "u:${USERID}:rwx" true

    if dir_has_required_access "$target_dir"; then
        return 0
    fi

    echo "Failed to obtain rwx access to $target_dir after ACL update." >&2
    return 1
}

ini_load_file "$CONFIG_FILE" build host env

BASE_IMAGE=$(ini_get_loaded_value build BASE_IMAGE "${BASE_IMAGE:-ubuntu:25.10}")
DEVCONTAINER_USER=${USER:-$(id -un)}
USERID=$(id -u)
GROUPID=$(id -g)
VIDEO_GID=$(resolve_group_id video '/dev/dri/card*')
RENDER_GID=$(resolve_group_id render '/dev/dri/renderD*')
WORKSPACE_BASENAME=$(basename "$WORKSPACE_DIR")
HOST_HOME=${HOME:-$(getent passwd "$DEVCONTAINER_USER" | cut -d: -f6)}
DEVCONTAINER_HOME="/home/${DEVCONTAINER_USER}"
SSH_SOURCE=$HOST_HOME/.ssh
GITCONFIG_SOURCE=$HOST_HOME/.gitconfig
HF_HOME_HOST_PATH=$(ini_get_loaded_value host HF_HOME "$HOST_HOME/.cache/huggingface")
CCACHE_HOST_PATH=$(ini_get_loaded_value host CCACHE_DIR "$HOST_HOME/.cache/ccache")
BUILD_HTTP_PROXY=${http_proxy:-}
BUILD_HTTPS_PROXY=${https_proxy:-}
BUILD_NO_PROXY=${no_proxy:-$DEFAULT_NO_PROXY}
DEVCONTAINER_INIT_INCLUDE=$(ini_get_loaded_value build INCLUDE "")

mkdir -p "$SSH_SOURCE"

if [ ! -f "$GITCONFIG_SOURCE" ]; then
    GITCONFIG_SOURCE=/tmp/devcontainer-empty-gitconfig
    : > "$GITCONFIG_SOURCE"
fi

ensure_mount_dir_access "$HF_HOME_HOST_PATH"
ensure_mount_dir_access "$CCACHE_HOST_PATH"

if docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
    echo "Use base image: $BASE_IMAGE"
else
    echo "Base image $BASE_IMAGE not found locally; Docker will try to resolve it during build."
fi

printf '# Generated by .devcontainer/scripts/init.sh\n' > "$RUNTIME_ENV_FILE"
append_runtime_entry DEVCONTAINER_INIT_INCLUDE "${DEVCONTAINER_INIT_INCLUDE:-}"
append_runtime_entry_if_set http_proxy "$BUILD_HTTP_PROXY"
append_runtime_entry_if_set https_proxy "$BUILD_HTTPS_PROXY"
append_runtime_entry_if_set no_proxy "$BUILD_NO_PROXY"
ini_for_each_loaded_section_entry env append_runtime_config_entry

sed \
    -e "s|__DEVCONTAINER_USER__|$(escape_sed_replacement "$DEVCONTAINER_USER")|g" \
    -e "s|__WORKSPACE_BASENAME__|$(escape_sed_replacement "$WORKSPACE_BASENAME")|g" \
    -e "s|__BASE_IMAGE__|$(escape_sed_replacement "$BASE_IMAGE")|g" \
    -e "s|__USERID__|$(escape_sed_replacement "$USERID")|g" \
    -e "s|__GROUPID__|$(escape_sed_replacement "$GROUPID")|g" \
    -e "s|__VIDEO_GID__|$(escape_sed_replacement "$VIDEO_GID")|g" \
    -e "s|__RENDER_GID__|$(escape_sed_replacement "$RENDER_GID")|g" \
    -e "s|__HOST_HOME__|$(escape_sed_replacement "$HOST_HOME")|g" \
    -e "s|__DEVCONTAINER_HOME__|$(escape_sed_replacement "$DEVCONTAINER_HOME")|g" \
    -e "s|__SSH_SOURCE__|$(escape_sed_replacement "$SSH_SOURCE")|g" \
    -e "s|__GITCONFIG_SOURCE__|$(escape_sed_replacement "$GITCONFIG_SOURCE")|g" \
    -e "s|__WORKSPACE_DIR__|$(escape_sed_replacement "$WORKSPACE_DIR")|g" \
    -e "s|__CCACHE_DIR__|$(escape_sed_replacement "$CCACHE_HOST_PATH")|g" \
    -e "s|__HF_HOME__|$(escape_sed_replacement "$HF_HOME_HOST_PATH")|g" \
    -e "s|__http_proxy__|$(escape_sed_replacement "$BUILD_HTTP_PROXY")|g" \
    -e "s|__https_proxy__|$(escape_sed_replacement "$BUILD_HTTPS_PROXY")|g" \
    -e "s|__no_proxy__|$(escape_sed_replacement "$BUILD_NO_PROXY")|g" \
    "$TEMPLATE_FILE" > "$COMPOSE_FILE"

exit 0