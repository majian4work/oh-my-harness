#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DEVCONTAINER_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
WORKSPACE_DIR=$(CDPATH= cd -- "$DEVCONTAINER_DIR/.." && pwd)
DEVCONTAINER_JSON="$DEVCONTAINER_DIR/devcontainer.json"
HELPER_SH="$DEVCONTAINER_DIR/scripts/helper.sh"

if [[ ! -f "$DEVCONTAINER_JSON" ]]; then
    echo "Missing $DEVCONTAINER_JSON" >&2
    exit 1
fi

if [[ ! -f "$HELPER_SH" ]]; then
    echo "Missing $HELPER_SH" >&2
    exit 1
fi

# shellcheck disable=SC1090
source "$HELPER_SH"

JSON_FILE=$(mktemp)
trap 'rm -f "$JSON_FILE"' EXIT
trim_comment "$DEVCONTAINER_JSON" > "$JSON_FILE"
COMPOSE_BIN=()

COMMAND=${1:-up}
shift || true

NO_BUILD=0
SKIP_POST_CREATE=0
FORCE_POST_CREATE=0
STALE_CONTAINER_DECISION=
STALE_CONTAINER_CHECKED_ID=
STALE_DEVCONTAINER_FILE=
STALE_DEVCONTAINER_MTIME=
STALE_CONTAINER_CREATED_AT=

usage() {
    cat <<'EOF'
Usage: ./.devcontainer/bin/devcontainer.sh [command] [options]

Commands:
  up            Build and start the devcontainer service, then run lifecycle hooks
  stop          Stop the devcontainer service
  down          Remove the devcontainer compose stack
  shell         Open the container's default login shell
  attach        Start the devcontainer if needed, ensure hooks ran, then enter dev shell
  logs          Show service logs
  ps            Show compose status
  post-create   Run postCreateCommand inside the container
  post-start    Run postStartCommand inside the container

Options for `up`:
  --no-build            Skip image build
  --skip-post-create    Do not run postCreateCommand
  --force-post-create   Run postCreateCommand even if it already ran before
  -h, --help            Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)
            NO_BUILD=1
            ;;
        --skip-post-create)
            SKIP_POST_CREATE=1
            ;;
        --force-post-create)
            FORCE_POST_CREATE=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
    shift
done

json_get() {
    local query=$1
    jq -r "$query // empty" "$JSON_FILE"
}

ensure_compose_bin() {
    if (( ${#COMPOSE_BIN[@]} > 0 )); then
        return 0
    fi

    if docker compose version >/dev/null 2>&1; then
        COMPOSE_BIN=(docker compose)
        return 0
    fi

    if command -v docker-compose >/dev/null 2>&1; then
        COMPOSE_BIN=(docker-compose)
        return 0
    fi

    echo "docker compose is required" >&2
    exit 1
}

resolve_path() {
    local base_dir=$1
    local path_value=$2
    if [[ "$path_value" = /* ]]; then
        printf '%s\n' "$path_value"
        return 0
    fi
    printf '%s/%s\n' \
        "$(CDPATH= cd -- "$base_dir" && CDPATH= cd -- "$(dirname -- "$path_value")" && pwd)" \
        "$(basename -- "$path_value")"
}

get_compose_files() {
    jq -r '
        .dockerComposeFile as $compose
        | if ($compose | type) == "array" then $compose[] else $compose end
    ' "$JSON_FILE"
}

INITIALIZE_COMMAND=$(json_get '.initializeCommand')
SERVICE_NAME=$(json_get '.service')
POST_CREATE_COMMAND=$(json_get '.postCreateCommand')
POST_START_COMMAND=$(json_get '.postStartCommand')

if [[ -z "$SERVICE_NAME" ]]; then
    echo "devcontainer.json is missing .service" >&2
    exit 1
fi

compose_args() {
    local compose_path
    local relative_path

    while IFS= read -r relative_path; do
        [[ -n "$relative_path" ]] || continue
        compose_path=$(resolve_path "$DEVCONTAINER_DIR" "$relative_path")
        printf '%s\0%s\0' -f "$compose_path"
    done < <(get_compose_files)
}

read_compose_args() {
    local -n target_ref=$1
    target_ref=()
    while IFS= read -r -d '' token; do
        target_ref+=("$token")
    done < <(compose_args)
}

get_primary_compose_file() {
    local relative_path

    while IFS= read -r relative_path; do
        [[ -n "$relative_path" ]] || continue
        resolve_path "$DEVCONTAINER_DIR" "$relative_path"
        return 0
    done < <(get_compose_files)

    return 1
}

get_service_workdir() {
    local compose_file

    compose_file=$(get_primary_compose_file) || return 1
    awk '
        $1 == "working_dir:" {
            print $2
            exit
        }
    ' "$compose_file"
}

run_initialize() {
    if [[ -z "$INITIALIZE_COMMAND" ]]; then
        return 0
    fi

    (
        cd "$WORKSPACE_DIR"
        bash -lc "$INITIALIZE_COMMAND"
    )
}

compose_ps_id() {
    local compose_opts=()
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" ps -q "$SERVICE_NAME"
}

compose_ps_any_id() {
    local compose_opts=()
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" ps -a -q "$SERVICE_NAME"
}

get_latest_devcontainer_change() {
    find "$DEVCONTAINER_DIR" -type f -printf '%T@ %p\n' | sort -nr | head -n 1
}

devcontainer_is_newer_than_container() {
    local container_id=$1
    local created_at
    local created_epoch
    local latest_change
    local latest_epoch

    created_at=$(docker inspect -f '{{.Created}}' "$container_id" 2>/dev/null) || return 1
    created_epoch=$(date -d "$created_at" '+%s' 2>/dev/null) || return 1
    latest_change=$(get_latest_devcontainer_change)

    [[ -n "$latest_change" ]] || return 1

    latest_epoch=${latest_change%% *}
    STALE_DEVCONTAINER_FILE=${latest_change#* }
    STALE_DEVCONTAINER_MTIME=$(date -d "@${latest_epoch%.*}" '+%Y-%m-%d %H:%M:%S %z')
    STALE_CONTAINER_CREATED_AT=$(date -d "$created_at" '+%Y-%m-%d %H:%M:%S %z')

    awk -v latest="$latest_epoch" -v created="$created_epoch" 'BEGIN { exit !(latest > created) }'
}

should_rebuild_devcontainer() {
    local container_id=$1
    local reply

    if [[ -z "$container_id" ]]; then
        return 1
    fi

    if [[ "$STALE_CONTAINER_CHECKED_ID" == "$container_id" ]]; then
        [[ "$STALE_CONTAINER_DECISION" == "rebuild" ]]
        return
    fi

    STALE_CONTAINER_CHECKED_ID=$container_id
    STALE_CONTAINER_DECISION=

    if ! devcontainer_is_newer_than_container "$container_id"; then
        return 1
    fi

    echo ".devcontainer has changed since the current container was created:"
    echo "  latest file: $STALE_DEVCONTAINER_FILE"
    echo "  modified at: $STALE_DEVCONTAINER_MTIME"
    echo "  container created at: $STALE_CONTAINER_CREATED_AT"

    if [[ "$NO_BUILD" -eq 1 ]]; then
        echo "--no-build was provided; continuing with the existing container."
        STALE_CONTAINER_DECISION=reuse
        return 1
    fi

    if [[ -t 0 ]]; then
        read -r -p "Rebuild the devcontainer now? [Y/n] " reply
        case "$reply" in
            [Nn]|[Nn][Oo])
                STALE_CONTAINER_DECISION=reuse
                echo "Continuing with the existing container."
                return 1
                ;;
            *)
                STALE_CONTAINER_DECISION=rebuild
                return 0
                ;;
        esac
    fi

    echo "No interactive terminal detected; rebuilding the devcontainer automatically."
    STALE_CONTAINER_DECISION=rebuild
    return 0
}

require_running_container_id() {
    local container_id

    container_id=$(compose_ps_id)
    if ! is_service_running "$container_id"; then
        echo "Service $SERVICE_NAME is not running" >&2
        exit 1
    fi

    printf '%s\n' "$container_id"
}

is_service_running() {
    local container_id=$1
    [[ -n "$container_id" ]] && docker inspect -f '{{.State.Running}}' "$container_id" 2>/dev/null | grep -qx true
}

run_in_service() {
    local command=$1
    local compose_opts=()
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" exec -T "$SERVICE_NAME" bash -lc "$command"
}

run_post_create() {
    if [[ -z "$POST_CREATE_COMMAND" ]]; then
        return 0
    fi

    if [[ "$FORCE_POST_CREATE" -eq 1 ]]; then
        run_in_service "DEVCONTAINER_FORCE_POST_CREATE=1 $POST_CREATE_COMMAND"
        return 0
    fi

    run_in_service "$POST_CREATE_COMMAND"
}

run_post_start() {
    if [[ -z "$POST_START_COMMAND" ]]; then
        return 0
    fi

    require_running_container_id >/dev/null
    run_in_service "$POST_START_COMMAND"
}

ensure_devcontainer_ready() {
    local container_id
    local existing_container_id

    run_initialize
    existing_container_id=$(compose_ps_any_id || true)

    if [[ -n "$existing_container_id" ]] && should_rebuild_devcontainer "$existing_container_id"; then
        run_up
        return 0
    fi

    container_id=$(compose_ps_id || true)

    if ! is_service_running "$container_id"; then
        run_up
        return 0
    fi

    run_post_create
    run_post_start
}

run_up() {
    local compose_opts=()
    local existing_container_id
    local should_build=1
    local up_args=(up -d)

    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    existing_container_id=$(compose_ps_any_id || true)

    if [[ "$NO_BUILD" -eq 1 ]]; then
        should_build=0
    elif [[ -n "$existing_container_id" ]] && ! should_rebuild_devcontainer "$existing_container_id" && [[ "$STALE_CONTAINER_DECISION" == "reuse" ]]; then
        should_build=0
    fi

    if [[ "$should_build" -eq 0 ]]; then
        up_args+=(--no-build)
    else
        up_args+=(--build)
    fi
    up_args+=("$SERVICE_NAME")

    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" "${up_args[@]}"

    if [[ "$SKIP_POST_CREATE" -eq 0 ]]; then
        run_post_create
    fi

    run_post_start
}

run_stop() {
    local compose_opts=()
    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" stop "$SERVICE_NAME"
}

run_down() {
    local compose_opts=()
    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" down
}

run_shell() {
    local compose_opts=()
    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" exec "$SERVICE_NAME" sh -lc '
        login_shell=${SHELL:-$(getent passwd "$(id -un)" | cut -d: -f7)}
        exec "$login_shell" -il
    '
}

run_attach() {
    local compose_opts=()
    local workspace_path

    ensure_devcontainer_ready
    ensure_compose_bin
    read_compose_args compose_opts
    workspace_path=$(get_service_workdir)

    if [[ -z "$workspace_path" ]]; then
        echo "Failed to determine container working directory" >&2
        exit 1
    fi

    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" exec "$SERVICE_NAME" sh -lc '
        set -e
        login_shell=${SHELL:-$(getent passwd "$(id -un)" | cut -d: -f7)}
        cd "'"$workspace_path"'"
        exec "$login_shell" -il
    '
}

run_logs() {
    local compose_opts=()
    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" logs -f "$SERVICE_NAME"
}

run_ps() {
    local compose_opts=()
    run_initialize
    ensure_compose_bin
    read_compose_args compose_opts
    "${COMPOSE_BIN[@]}" "${compose_opts[@]}" ps
}

case "$COMMAND" in
    up)
        run_up
        ;;
    stop)
        run_stop
        ;;
    down)
        run_down
        ;;
    shell)
        run_shell
        ;;
    attach)
        run_attach
        ;;
    logs)
        run_logs
        ;;
    ps)
        run_ps
        ;;
    post-create)
        run_initialize
        run_post_create
        ;;
    post-start)
        run_initialize
        run_post_start
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "Unknown command: $COMMAND" >&2
        usage >&2
        exit 1
        ;;
esac