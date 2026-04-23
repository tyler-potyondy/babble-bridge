FROM --platform=linux/amd64 ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG USERNAME=nrf-rpc-dev
ARG USER_UID=1000
ARG USER_GID=1000

# Base dependencies for Zephyr, west, BabbleSim, and general development
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    cmake \
    ninja-build \
    gperf \
    ccache \
    dfu-util \
    device-tree-compiler \
    wget \
    python3 \
    python3-dev \
    python3-pip \
    python3-venv \
    make \
    gcc \
    gcc-multilib \
    g++ \
    g++-multilib \
    libsdl2-dev \
    libxml2-dev \
    xz-utils \
    file \
    ca-certificates \
    curl \
    socat \
    && rm -rf /var/lib/apt/lists/*

# Install Zephyr SDK
ARG ZEPHYR_SDK_VERSION=0.17.4
RUN wget -q https://github.com/zephyrproject-rtos/sdk-ng/releases/download/v${ZEPHYR_SDK_VERSION}/zephyr-sdk-${ZEPHYR_SDK_VERSION}_linux-$(uname -m).tar.xz \
    && tar xf zephyr-sdk-${ZEPHYR_SDK_VERSION}_linux-$(uname -m).tar.xz -C /opt \
    && rm zephyr-sdk-${ZEPHYR_SDK_VERSION}_linux-$(uname -m).tar.xz \
    && /opt/zephyr-sdk-${ZEPHYR_SDK_VERSION}/setup.sh -c
ENV ZEPHYR_SDK_INSTALL_DIR=/opt/zephyr-sdk-${ZEPHYR_SDK_VERSION}

# Create a non-root user and group, if user/group uid/gid already exists then just alter.
RUN groupadd -g ${USER_GID} ${USERNAME} || groupmod -n ${USERNAME} $(getent group ${USER_GID} | cut -d: -f1) \
    && useradd -m -u ${USER_UID} -g ${USER_GID} -s /bin/bash ${USERNAME} || usermod -l ${USERNAME} -u ${USER_UID} -g ${USER_GID} $(getent passwd ${USER_UID} | cut -d: -f1)

# Switch to non-root user
USER ${USERNAME}

# Install Rust for the non-root user
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/home/${USERNAME}/.cargo/bin:${PATH}"

# Set workspace
WORKDIR /workspace
