# syntax=docker/dockerfile:1.7

FROM ubuntu:26.04 AS package

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_TARGET=x86_64-unknown-linux-gnu
ARG DEB_ARCH=amd64
ARG SOURCE_DATE_EPOCH=0

ENV PATH="/usr/local/cargo/bin:${PATH}"
ENV CARGO_HOME="/usr/local/cargo"
ENV RUSTUP_HOME="/usr/local/rustup"
ENV RUST_TARGET="$RUST_TARGET"
ENV DEB_ARCH="$DEB_ARCH"
ENV SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH"
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="aarch64-linux-gnu-gcc"
ENV CC_aarch64_unknown_linux_gnu="aarch64-linux-gnu-gcc"
ENV AR_aarch64_unknown_linux_gnu="aarch64-linux-gnu-ar"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        dpkg-dev \
        gcc-aarch64-linux-gnu \
        git \
        gzip \
        libc6-dev-arm64-cross \
        pkg-config \
        rustup \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY rust-toolchain.toml .
RUN rustup show

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    packaging/build-deb-in-container.sh

FROM scratch AS artifact

COPY --from=package /src/dist/ /
