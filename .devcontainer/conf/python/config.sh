#!/bin/bash

# vscode extensions
# declare -a exts=(
#     ms-python.python
#     ms-python.vscode-pylance
# )
# for ext in "${exts[@]}"; do
#     code --install-extension "$ext"
# done

# config
CUR_DIR=$(dirname "${BASH_SOURCE[0]}")
source ${SCRIPT_DIR}/helper.sh
# merge_vsconf "${CUR_DIR}/vscode/*" "${WORKSPACE_DIR}/.vscode"

# pip
# mkdir -p $HOME/.pip
# safe_link ${CUR_DIR}/pip.conf $HOME/.pip/pip.conf

# python dev env
save_env 'VIRTUAL_ENV=$HOME/.venv'
save_env 'PATH=$HOME/.local/bin:$VIRTUAL_ENV/bin:$PATH'
curl -LsSf https://astral.sh/uv/install.sh | UV_NO_MODIFY_PATH=1 sh

save_env 'UV_PYTHON_INSTALL_DIR=$HOME/.uv/python'
save_env 'UV_LINK_MODE=copy'
save_env 'UV_HTTP_TIMEOUT=500'
PYTHON_VERSION=3.12
uv venv --python ${PYTHON_VERSION} --seed ${VIRTUAL_ENV}

PIP_EXTRA_INDEX_URL="https://download.pytorch.org/whl/xpu"
save_env 'PIP_EXTRA_INDEX_URL=https://download.pytorch.org/whl/xpu'
save_env 'UV_INDEX=$PIP_EXTRA_INDEX_URL'
save_env 'UV_INDEX_STRATEGY=unsafe-best-match'
