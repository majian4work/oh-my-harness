#!/bin/bash

# https://stackoverflow.com/questions/59895/how-do-i-get-the-directory-where-a-bash-script-is-located-from-within-the-script
SCRIPT_DIR=$(dirname "${BASH_SOURCE[0]}")
DEVCONTAINER_DIR=$(cd -- "$SCRIPT_DIR/.." &> /dev/null && pwd )
CONF_DIR=$(cd -- "$DEVCONTAINER_DIR/conf" &> /dev/null && pwd )
WORKSPACE_DIR=$(cd -- "$DEVCONTAINER_DIR/.." &> /dev/null && pwd )

source ${SCRIPT_DIR}/helper.sh

POST_CREATE_MARKER=${HOME}/.devcontainer-postcreate.done

if [[ "${DEVCONTAINER_FORCE_POST_CREATE:-0}" != "1" ]] && [[ -f "$POST_CREATE_MARKER" ]]; then
    exit 0
fi

append_line_if_missing() {
    local file_path=$1
    local line=$2

    touch "$file_path"
    if ! grep -Fqx "$line" "$file_path"; then
        printf '%s\n' "$line" >> "$file_path"
    fi
}

# "docker run --hostname=dev" not add entry into /etc/hosts
echo $(hostname -I | cut -d\  -f1) $(hostname) | sudo tee -a /etc/hosts

if [ -d "$HOME/.host" ]; then
    # "ssh -X" change inode of ".Xauthority", so it can't keep sync with host, then authentication will faild
    # https://medium.com/@jonsbun/why-need-to-be-careful-when-mounting-single-files-into-a-docker-container-4f929340834
    safe_link ${HOME}/.host/.Xauthority ${HOME}/.Xauthority
fi
# hack X11 forwarding
save_env 'DISPLAY=$(hostname):10'

unset_env 'HTTP_PROXY'
unset_env 'HTTPS_PROXY'
unset_env 'ALL_PROXY'
unset_env 'NO_PROXY'
if [ -n "$http_proxy" ]; then
    echo "use_proxy=yes" >> ~/.wgetrc
    echo "http_proxy=$http_proxy" >> ~/.wgetrc
    echo "https_proxy=$http_proxy" >> ~/.wgetrc
fi

# "apt-get update" should ahead any "apt-get install" in other scripts
sudo apt-get update
# for perf tool, `uname -r` not work when host is not ubuntu
sudo apt-get install -y linux-tools-common linux-tools-generic #linux-tools-`uname -r`

# workspace common vscode conf
install_vsconf ${CONF_DIR}/workspace ${WORKSPACE_DIR}

# extra conf from $DEVCONTAINER_INIT_INCLUDE
if [ -n "$DEVCONTAINER_INIT_INCLUDE" ]; then
    IFS=',' read -ra INCLUDES <<< "$DEVCONTAINER_INIT_INCLUDE"
    for SUB_CONF in "${INCLUDES[@]}"; do
        install_vsconf ${CONF_DIR}/${SUB_CONF} ${WORKSPACE_DIR}
    done
fi

mkdir -p $HOME/.local/bin
save_env 'PATH=$HOME/.local/bin:$PATH'

sh -c "$(curl -fsSL https://starship.rs/install.sh)" -y -f -b  $HOME/.local/bin
printf '\n%s\n' 'eval "$(starship init bash)"' >> ~/.bashrc
printf '\n%s\n' 'eval "$(starship init zsh)"' >> ~/.zshrc

# mise: tools manager, need to config shell
curl https://mise.run | sh
save_env 'PATH=$HOME/.local/share/mise/shims:$PATH'
append_line_if_missing "$HOME/.bashrc" 'eval "$(mise activate bash)"'
append_line_if_missing "$HOME/.zshrc" 'eval "$(mise activate zsh)"'
eval "$(mise activate bash)"
mise trust
mise install
mise reshim

# AI
curl -fsSL https://opencode.ai/install | bash
# opencode plugin -g oh-my-openagent@latest
# curl -fsSL https://bun.com/install | bash
# save_env 'PATH=$HOME/.bun/bin:$PATH'
# bun install -g oh-my-openagent@latest
# bunx oh-my-openagent install --no-tui --claude=no --gemini=no --copilot=yes --skip-auth

# curl -fsSL https://gh.io/copilot-install | bash
# curl -fsSL https://claude.ai/install.sh | bash

touch "$POST_CREATE_MARKER"