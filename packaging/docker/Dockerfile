# Chanto's container image, published to ghcr.io/chanto-sh/chanto by the
# release workflow. It is built from the release's own static musl binaries,
# not recompiled: packaging/docker/prepare.sh checks them against the signed
# SHA256SUMS and puts them in the build context as bin/<arch>/chanto.
#
# Debian stable-slim, pinned by digest (dependabot keeps it current): glibc
# and apt, so an image built FROM this one can add a project's toolchain with
# the usual packages, and Debian's security updates for git and its
# libraries. The chanto binary itself is static and doesn't depend on it.
FROM debian:stable-slim@sha256:a717a33469731894e77e912ea7d714e27c2dc38ee6554fd1a3102e92999cfc66

# What the agent's tools use: git for checkpoints, diffs and worktrees,
# ripgrep for Grep, and the CA bundle for HTTPS to model providers. bash and
# coreutils are already in the base image. The project is mounted from the
# host, usually owned by another uid, so git is told /work is safe.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 --user-group --shell /bin/bash chanto \
    && mkdir /work \
    && chown chanto:chanto /work \
    && git config --system --add safe.directory /work

ARG TARGETARCH
COPY --chmod=755 bin/${TARGETARCH}/chanto /usr/local/bin/chanto

# Updates come from pulling a newer image, not from `chanto upgrade`.
ENV CHANTO_NO_UPDATE_CHECK=1

USER chanto
WORKDIR /work
ENTRYPOINT ["chanto"]

ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.title="chanto" \
      org.opencontainers.image.description="Chanto, a terminal coding agent" \
      org.opencontainers.image.source="https://github.com/chanto-sh/chanto" \
      org.opencontainers.image.url="https://chanto.sh" \
      org.opencontainers.image.documentation="https://github.com/chanto-sh/chanto/blob/main/docs/reference.md#container-image" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.licenses="Apache-2.0"
