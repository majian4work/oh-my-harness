#!/bin/bash

RED="\033[1;31m"
NC="\033[0m"

# save env to bashrc/zshrc, so it can be used in vscode tasks and debug.
function save_env() {
    local env_entry="$1"
    local env_file

    if [[ -z "$env_entry" ]]; then
        return 1
    fi

    # test current shell, if zsh, save to .zshrc, else save to .bashrc
    if [[ "$SHELL" == *"zsh"* ]]; then
        env_file="$HOME/.zshrc"
    else
        env_file="$HOME/.bashrc"
    fi

    eval "export $env_entry"

    if [[ ! -f "$env_file" ]] || ! grep -Fqx "export $env_entry" "$env_file"; then
        printf '%s\n' "export $env_entry" >> "$env_file"
    fi
}

function trim_comment() {
    # can trim "//xxx" in "http://xxx"
    # "s|^[ \t]*//.*$||" all comment line
    # "s|[ \t]\+//.*$||" comment at the end of line
    sed -e "s|^[ \t]*//.*$||" -e "s|[ \t]\+//.*$||" $1 | sed "/^$/d"
}

function merge_json() {
    jq -s add <(trim_comment $1) <(trim_comment $2)
}

function merge_nested_arr() {
    jq -s '[.[] | to_entries] | flatten | reduce .[] as $dot ({}; .[$dot.key] += $dot.value)' <(trim_comment $1) <(trim_comment $2)
    # jq -s '.[0] + .[1]' <(trim_comment $1) <(trim_comment $2)
}

function merge_vsconf() {
    SRC=$1
    TARGET=$2
    mkdir -p ${TARGET}
    for SRC_PATH in $SRC; do
        if test -f ${SRC_PATH}; then
            FILE="$(basename -- ${SRC_PATH})"
            TARGET_PATH=${TARGET}/${FILE}
            if [[ -f ${TARGET_PATH} ]]; then
                TMP=${TARGET_PATH}.tmp
                if [[ "$FILE" == "extensions.json" ]]; then
                    merge_nested_arr ${TARGET_PATH} ${SRC_PATH} > ${TMP}
                else
                    merge_json ${TARGET_PATH} ${SRC_PATH} > ${TMP}
                fi
                if [[ $? != 0 ]]; then
                    echo -e "${RED}Failed to merge ${SRC_PATH} > ${TARGET_PATH}${NC}"
                    rm ${TMP}
                    continue
                fi
                rm ${TARGET_PATH}
                mv ${TMP} ${TARGET_PATH}
            else
                trim_comment ${SRC_PATH} > ${TARGET_PATH}
            fi
        fi
    done
}

function install_vsconf() {
    SRC_DIR=$1
    WORKSPACE_DIR=$2

    # config.sh can modify vscode config, eg. replace env variable
    source ${SRC_DIR}/config.sh
    # jq '.recommendations[]' ${SRC_DIR}/vscode/extensions.json | xargs -L 1 code --install-extension
    merge_vsconf "${SRC_DIR}/vscode/*" "${WORKSPACE_DIR}/.vscode"
}


function safe_link() {
    SRC=$1
    TARGET=$2
    ([[ -e ${TARGET} ]] && echo -e "${RED}${TARGET} already exist${NC}") || ln -s ${SRC} ${TARGET}
}

function current_container_id() {
    local container_id

    container_id=$(awk '
        match($0, /\/var\/lib\/docker\/containers\/([0-9a-f]{64})\//, matches) {
            print matches[1]
            exit
        }
    ' /proc/self/mountinfo 2>/dev/null)

    if [[ -n "$container_id" ]]; then
        printf '%s\n' "$container_id"
        return 0
    fi

    return 1
}

declare -ga INI_ORDER=()
declare -gA INI_VALUES=()
declare -g INI_LOADED_FILE=

function ini_trim_whitespace() {
    printf '%s' "$1" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

function ini_strip_wrapping_quotes() {
    local value=$1

    case "$value" in
        \"*\")
            value=${value#\"}
            value=${value%\"}
            ;;
        \'*\')
            value=${value#\'}
            value=${value%\'}
            ;;
    esac

    printf '%s' "$value"
}

function ini_expand_value() {
    local value=$1

    case "$value" in
        '~')
            value=$HOME
            ;;
        '~/'*)
            value=$HOME/${value#~/}
            ;;
    esac

    printf '%s' "$value"
}

function ini_is_allowed_section() {
    local target_section=$1
    shift

    local allowed_section
    for allowed_section in "$@"; do
        [[ "$allowed_section" == "$target_section" ]] && return 0
    done

    return 1
}

function ini_validate_key_name() {
    local key_name=$1

    case "$key_name" in
        ''|[0-9]*|*[!A-Za-z0-9_]* )
            return 1
            ;;
    esac

    return 0
}

function ini_reset_loaded_config() {
    unset INI_ORDER INI_VALUES INI_LOADED_FILE
    declare -ga INI_ORDER=()
    declare -gA INI_VALUES=()
    declare -g INI_LOADED_FILE=
}

function ini_load_file() {
    local file_path=$1
    shift
    local allowed_sections=("$@")
    local current_section=
    local line_number=0
    local raw_line
    local line
    local section_name
    local key_name
    local value
    local entry_id

    ini_reset_loaded_config
    INI_LOADED_FILE=$file_path

    if [[ ! -f "$file_path" ]]; then
        return 0
    fi

    while IFS= read -r raw_line || [[ -n "$raw_line" ]]; do
        ((line_number += 1))
        line=$(ini_trim_whitespace "$raw_line")

        case "$line" in
            ''|'#'*|';'*)
                continue
                ;;
            '['*']')
                section_name=${line#'['}
                section_name=${section_name%']'}
                section_name=$(ini_trim_whitespace "$section_name")
                if ! ini_is_allowed_section "$section_name" "${allowed_sections[@]}"; then
                    echo "Ignoring unknown INI section [$section_name] in $file_path:$line_number" >&2
                    current_section=__ignored_section__
                    continue
                fi
                current_section=$section_name
                continue
                ;;
        esac

        if [[ -z "$current_section" ]]; then
            echo "Ignoring INI entry outside any section in $file_path:$line_number" >&2
            continue
        fi

        if [[ "$current_section" == __ignored_section__ ]]; then
            continue
        fi

        if [[ "$line" != *=* ]]; then
            echo "Ignoring malformed INI line in $file_path:$line_number" >&2
            continue
        fi

        key_name=$(ini_trim_whitespace "${line%%=*}")
        if ! ini_validate_key_name "$key_name"; then
            echo "Ignoring invalid INI key [$current_section] $key_name in $file_path:$line_number" >&2
            continue
        fi

        value=$(ini_trim_whitespace "${line#*=}")
        value=$(ini_strip_wrapping_quotes "$value")
        value=$(ini_expand_value "$value")
        entry_id="$current_section.$key_name"

        if [[ -v INI_VALUES[$entry_id] ]]; then
            echo "Duplicate INI key [$current_section] $key_name in $file_path:$line_number; overriding previous value" >&2
        else
            INI_ORDER+=("$entry_id")
        fi

        INI_VALUES[$entry_id]=$value
    done < "$file_path"
}

function ini_get_loaded_value() {
    local section_name=$1
    local key_name=$2
    local default_value=${3:-}
    local entry_id="$section_name.$key_name"

    if [[ -v INI_VALUES[$entry_id] ]]; then
        printf '%s' "${INI_VALUES[$entry_id]}"
    else
        printf '%s' "$default_value"
    fi
}

function ini_for_each_loaded_section_entry() {
    local target_section=$1
    local callback=$2
    local entry_id
    local section_name
    local key_name

    for entry_id in "${INI_ORDER[@]}"; do
        section_name=${entry_id%%.*}
        [[ "$section_name" == "$target_section" ]] || continue
        key_name=${entry_id#*.}
        "$callback" "$key_name" "${INI_VALUES[$entry_id]}"
    done
}