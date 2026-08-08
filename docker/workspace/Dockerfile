FROM node:22.23.1-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3

LABEL org.opencontainers.image.source="https://github.com/CorVous/CorCode"
LABEL org.opencontainers.image.description="CorCode workspace: Claude Code behind the ACP adapter, with Node, Rust and Python toolchains."

ARG DEBIAN_FRONTEND=noninteractive
ARG GH_VERSION=2.97.0
ARG GH_DEB_SHA256=7c7fa3bb890db0934baf65910d97b8c0fa437b2e590f7f7daf6bdf82c5c486d7
ARG RUSTUP_VERSION=1.29.0
ARG RUSTUP_INIT_SHA256=4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10
ARG RUST_VERSION=1.97.1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      curl \
      git \
      jq \
      python3 \
      python3-pip \
      python3-venv \
      ripgrep \
 && curl -fsSL -o /tmp/gh.deb \
      "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.deb" \
 && echo "${GH_DEB_SHA256}  /tmp/gh.deb" | sha256sum -c - \
 && apt-get install -y --no-install-recommends /tmp/gh.deb \
 && rm -f /tmp/gh.deb \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl -fsSL -o /tmp/rustup-init \
      "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
 && echo "${RUSTUP_INIT_SHA256}  /tmp/rustup-init" | sha256sum -c - \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain stable \
 && rustup component add rustfmt clippy \
 && rm -f /tmp/rustup-init \
 && installed="$(rustc --version | cut -d' ' -f2)" \
 && if [ "$installed" != "${RUST_VERSION}" ]; then \
      echo "stable Rust has moved: expected ${RUST_VERSION}, got ${installed}" >&2; \
      exit 1; \
    fi

WORKDIR /opt/corcode-agent
COPY package.json package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund \
 && npm cache clean --force \
 && ln -s /opt/corcode-agent/node_modules/.bin/claude-agent-acp /usr/local/bin/claude-agent-acp

COPY verify-sdk-pin.sh ./
RUN bash verify-sdk-pin.sh

COPY managed-settings.json /etc/claude-code/managed-settings.json
COPY stop-hook.sh /usr/local/bin/corcode-stop-hook
RUN chmod 0755 /usr/local/bin/corcode-stop-hook

RUN groupmod --new-name agent node \
 && usermod --login agent --home /home/agent --move-home --shell /bin/bash node \
 && mkdir -p /workspace /home/agent/.claude \
 && chown agent:agent /workspace /home/agent/.claude

USER agent
ENV HOME=/home/agent \
    CLAUDE_CONFIG_DIR=/home/agent/.claude

# The runtime rootfs is read-only and the only writable places are the tmpfs
# scratch and the two binds (ADR-0001), so every toolchain cache is sent to
# scratch. The baked Rust toolchain stays where it is, readable: only
# CARGO_HOME, which is what cargo writes through, moves.
ENV CARGO_HOME=/tmp/cache/cargo \
    npm_config_cache=/tmp/cache/npm \
    PIP_CACHE_DIR=/tmp/cache/pip \
    XDG_CACHE_HOME=/tmp/cache/xdg \
    GH_CONFIG_DIR=/tmp/cache/gh

# The agent pushes its own commits (ADR-0005) over the token the core spawns
# it with. gh turns that token into a credential; the helper is scoped to
# github.com so no other host is ever offered one, the way the core splices a
# token into github.com URLs and nothing else. A container spawned without the
# token answers nothing here, and the stop hook stands down.
RUN git config --global user.name 'Cor' \
 && git config --global user.email 'birb@cor.gg' \
 && git config --global --add safe.directory /workspace \
 && git config --global 'credential.https://github.com.helper' '!gh auth git-credential'

WORKDIR /workspace
CMD ["claude-agent-acp"]
