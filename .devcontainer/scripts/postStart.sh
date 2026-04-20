#!/bin/bash

SCRIPT_DIR=$(dirname "${BASH_SOURCE[0]}")
DEVCONTAINER_DIR=$(cd -- "$SCRIPT_DIR/.." &> /dev/null && pwd )
WORKSPACE_DIR=$(cd -- "$DEVCONTAINER_DIR/.." &> /dev/null && pwd )

source ${SCRIPT_DIR}/helper.sh

POST_START_MARKER=${HOME}/.devcontainer-poststart.container
CURRENT_CONTAINER_ID=$(current_container_id || true)

if [[ -n "$CURRENT_CONTAINER_ID" ]] \
	&& [[ -f "$POST_START_MARKER" ]] \
	&& [[ "$(cat "$POST_START_MARKER" 2>/dev/null)" == "$CURRENT_CONTAINER_ID" ]]; then
	exit 0
fi

# export PATH=`ls -t /vscode/vscode-server/bin/linux-x64/*/bin/remote-cli | head -n1`:$PATH
# export VSCODE_IPC_HOOK_CLI=`ls -t /tmp/vscode-ipc-*.sock | head -n1`
# jq '.recommendations[]' ${WORKSPACE_DIR}/.vscode/extensions.json | xargs -L 1 code --install-extension

if [[ -n "$CURRENT_CONTAINER_ID" ]]; then
	printf '%s\n' "$CURRENT_CONTAINER_ID" > "$POST_START_MARKER"
fi