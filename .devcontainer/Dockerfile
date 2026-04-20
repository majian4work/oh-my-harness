# syntax=docker/dockerfile:1.4
ARG BASE_IMAGE=ubuntu:25.10
FROM ${BASE_IMAGE} AS base

USER root
WORKDIR /
SHELL ["/bin/bash", "-c"]
ENV DEBIAN_FRONTEND=nointeractive

ARG http_proxy
ARG https_proxy
ARG no_proxy
RUN <<EOF
default_no_proxy='localhost,127.0.0.1/8,10.0.0.0/8'

effective_no_proxy="$default_no_proxy"
if [[ -n "$no_proxy" ]]; then
    effective_no_proxy="$no_proxy,$default_no_proxy"
fi

cat <<EOS >> /etc/environment
no_proxy="$effective_no_proxy"
EOS

if [[ -n "$http_proxy" ]]; then
    cat <<EOS >> /etc/environment
http_proxy="$http_proxy"
https_proxy="$http_proxy"
EOS
    cat <<EOS >> /etc/apt/apt.conf
Acquire::http::Proxy "$http_proxy";
Acquire::https::Proxy "$https_proxy";
EOS
else
    sed -i 's|archive.ubuntu.com|mirrors.163.com|' /etc/apt/sources.list
    sed -i 's|security.ubuntu.com|mirrors.163.com|' /etc/apt/sources.list
fi
EOF

RUN <<EOF
rm -f /etc/apt/apt.conf.d/docker-clean
echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache
EOF

# timezone, locale and sudo
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked <<EOF
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates tzdata locales sudo zsh
EOF

RUN <<EOF
ln -snf /usr/share/zoneinfo/PRC /etc/localtime
dpkg-reconfigure -f noninteractive tzdata
EOF

RUN <<EOF
sed -i '/en_US.UTF-8/s|^# ||g' /etc/locale.gen
locale-gen
EOF

# common softwares
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked <<EOF
apt-get install -y --no-install-recommends \
    build-essential ccache curl fontconfig git gnupg jq less \
    netcat-openbsd stow tar unzip vim wget xz-utils zip
EOF

ARG USER=vscode
ARG USERID=1000
ARG GROUPID=1000
RUN <<EOF
userdel -r ubuntu
groupadd -g ${GROUPID} ${USER}
useradd -m -u ${USERID} -g ${GROUPID} -s /usr/bin/zsh ${USER}
mkdir -p /home/${USER}/.cache/huggingface
chown -R ${USER}:${USER} /home/${USER}/.cache
echo "${USER} ALL=(root) NOPASSWD:ALL" > /etc/sudoers.d/${USER}
EOF
USER $USER
WORKDIR /home/${USER}
