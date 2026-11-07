# Worker image of stage direct, linux/arm64: the python and node tool sets, and for LANG_FAMILY=direct Go,
# the JDK, Maven and the slim Rust toolchain with the C toolchain it links through.
# Built only by images/codebuild/worker-python.yml on CodeBuild ARM, which first places the pinned
# binaries under build/: agentd (cross-built for aarch64 glibc 2.34), the
# promoted Cline fork artifact, the OpenCode 1.18.32 release binary, and the gitleaks 8.30.1 and
# uv 0.12.18 release binaries for the tool set. Every binary is checked against its SHA-256
# here; no image build compiles the Cline fork. Binaries are copied with their final mode and the
# build inputs are bind-mounted, so no layer rewrites or keeps them.
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:381352c430f36d8aa6b8b670de06e7e4e5642fe0cb9dbd463bb8d1e79aefc0c7

ARG AGENTD_SHA256
ARG CLINE_SHA256
ARG OPENCODE_SHA256
ARG GITLEAKS_SHA256
ARG UV_SHA256
ARG RG_SHA256
ARG SOURCE_SHA=unknown
# The language tool set (images/tools/lang/install.sh): `python` (Node 24 and the JS tools); `direct` adds Go 1.27,
# the JDK 21, Maven and the slim Rust set (rustc, cargo, the standard library, clippy, rustfmt, cargo-nextest), so one image builds and
# tests a Go, a Java, a Rust and a TypeScript project. scripts/deploy.sh sets it for stage direct (LANG_FAMILY of the build spec).
ARG LANG_FAMILY=python

# The direct family also gets the C toolchain rustc links through: gcc is `cc` (and compiles the C of build scripts, as the `cc` crate does for
# aws-lc-sys), binutils is the linker `ld`, glibc-devel holds the C library's start files and headers. These three are the smallest set that links an
# aarch64 Rust program; the python family adds nothing. Every family also gets the small tools a repository's own check script reaches for and a
# session may find missing: diff (diffutils), patch and jq, under 2 MB together.
RUN dnf install -y --setopt=install_weak_deps=False \
      git util-linux shadow-utils procps-ng iproute findutils tar gzip xz unzip which \
      diffutils patch jq \
      python3.12 python3.12-pip \
      $([ "$LANG_FAMILY" = direct ] && echo gcc binutils glibc-devel) \
    && dnf clean all && rm -rf /var/cache/dnf

# Native services: PostgreSQL 16, MariaDB 11.4, Valkey 9 and mailpit from images/services/install.sh (ham-services starts PostgreSQL in a
# session); the uid svc (904) and /var/lib/harness-services are the layout of the removed harness-gates `--services native`.
RUN --mount=type=bind,source=images/services,target=/svc bash /svc/install.sh \
    && useradd --system --uid 904 --user-group --home-dir /var/empty --no-create-home --shell /sbin/nologin svc \
    && mkdir -p /var/lib/harness-services && chmod 0711 /var/lib/harness-services

# uid harness runs agentd's state; uid agent runs the engine; uid sut runs repository
# code under test.
RUN useradd --system --uid 900 --user-group --home-dir /var/lib/agentd --shell /sbin/nologin harness \
    && useradd --uid 901 --user-group --create-home --shell /bin/bash agent \
    && useradd --system --uid 903 --user-group --home-dir /var/empty --no-create-home --shell /sbin/nologin sut

COPY --chmod=0755 build/agentd build/cline /opt/harness/bin/
COPY engine/profiles/ /opt/harness/engine/profiles/

# Cline's goal plugin (one bundle, no Node packages) and the harness's observer plugin, with the lock agentd checks them against. agentd copies
# them into the engine's read-only configuration directory at the start of a run. The goal folder is not in the
# repository: `scripts/engine-fetch build goal-plugin` in images/codebuild/worker-python.yml fetches its pinned sources, bundles them and checks
# the five files against engine/plugins/lock.json before this build starts. About 16 KB; the cap is untouched.
COPY build/plugins/goal/ /opt/harness/engine/plugins/goal/
COPY engine/plugins/harness-rounds/ /opt/harness/engine/plugins/harness-rounds/
COPY engine/plugins/lock.json /opt/harness/engine/plugins/lock.json

# The Cline fork binary and the goal bundle are Apache-2.0 software of Cline Bot Inc.: the licence text (the same text for cline/cline and cline/plugins,
# checked by the build) and the repository notice, which names both and the changes, travel with the image.
COPY --chmod=0444 LICENSE NOTICE /opt/harness/licenses/
COPY --chmod=0444 build/plugins/goal/LICENSE /opt/harness/licenses/Apache-2.0-Cline.txt

# OpenCode (176 MiB) is installed from a bind mount, so no layer keeps a second copy, and not at all for the direct family: the one-engine
# release runs Cline only and the room is what the in-session container runtime and the Rust toolchain need.
RUN --mount=type=bind,source=build,target=/build \
    if [ "$LANG_FAMILY" != direct ]; then install -m 0755 /build/opencode /opt/harness/bin/opencode; fi

RUN cd /opt/harness/bin \
    && test -n "$AGENTD_SHA256" && test -n "$CLINE_SHA256" \
    && printf '%s  agentd\n%s  cline\n' "$AGENTD_SHA256" "$CLINE_SHA256" | sha256sum -c - \
    && if [ "$LANG_FAMILY" != direct ]; then test -n "$OPENCODE_SHA256" && printf '%s  opencode\n' "$OPENCODE_SHA256" | sha256sum -c -; fi \
    && mkdir -p /run/agentd /var/lib/agentd /mnt/workspace

# The engine's shell tools: ripgrep (OpenCode's glob and grep run it), and a python3 that is 3.12
# with pytest from the gate tool set's hash-checked wheels, so the worker can run the repository's
# tests in its session. Installed after the tool set below; see the RUN that follows it.
COPY --chmod=0755 build/rg /usr/local/bin/rg
# `pip` and `pip3` for the engine's PATH: the image's Python 3.12 has its pip module but no pip command on any PATH.
COPY --chmod=0755 images/worker-python/pip /usr/local/bin/pip
COPY --chmod=0755 images/worker-python/pip /usr/local/bin/pip3
RUN cd /usr/local/bin && test -n "$RG_SHA256" && printf '%s  rg\n' "$RG_SHA256" | sha256sum -c - \
    && rg --version

# The gate tool set (images/tools/install.sh, the same pins as the build
# machine's HAMH_TOOLS/gate-tools) at /opt/harness/gate-tools (HARNESS_GATES_TOOLS), and the
# language tool set (images/tools/lang/install.sh: Node 24 and the pinned JS tools)
# at /opt/harness/gate-tools/lang. Its bin directory (node, go, java, mvn, cargo and the other Rust commands) is on the engine's PATH.
RUN --mount=type=bind,source=build,target=/build --mount=type=bind,source=images/tools,target=/gate-src \
    test -n "$GITLEAKS_SHA256" && test -n "$UV_SHA256" \
    && printf '%s  /build/gitleaks\n%s  /build/uv\n' "$GITLEAKS_SHA256" "$UV_SHA256" | sha256sum -c - \
    && bash /gate-src/install.sh /opt/harness/gate-tools /usr/bin/python3.12 /build/uv /build/gitleaks \
    && TMPDIR=/tmp bash /gate-src/lang/install.sh /opt/harness/gate-tools/lang "$LANG_FAMILY" \
    && /usr/bin/python3.12 -m pip install --quiet --root-user-action=ignore --break-system-packages --no-index --find-links /opt/harness/gate-tools/wheels pytest \
    && ln -sf /usr/bin/python3.12 /usr/local/bin/python3 && ln -sf /usr/bin/python3.12 /usr/local/bin/python \
    && pip --version \
    && python3 -m pytest --version \
    && /opt/harness/gate-tools/lang/bin/node --version \
    && if [ "$LANG_FAMILY" = direct ]; then \
         PATH=/opt/harness/gate-tools/lang/bin:$PATH \
         && go version && java -version && mvn -v \
         && rustc --version && cargo --version && cargo nextest --version && cargo clippy --version && cargo fmt --version; \
       fi \
    && chmod -R go-w /opt/harness/gate-tools \
    && printf '{"agentd":"%s","cline":"%s","opencode":"%s","gitleaks":"%s","uv":"%s","gateTools":"%s","langTools":"%s","services":"%s","source":"%s"}\n' \
         "$AGENTD_SHA256" "$CLINE_SHA256" "$OPENCODE_SHA256" "$GITLEAKS_SHA256" "$UV_SHA256" \
         "$(sha256sum /opt/harness/gate-tools/manifest.json | cut -d' ' -f1)" \
         "$(sha256sum /opt/harness/gate-tools/lang/manifest.json | cut -d' ' -f1)" "$(sha256sum /opt/harness/services/manifest.json | cut -d' ' -f1)" "$SOURCE_SHA" > /opt/harness/pins.json

# The direct family also carries RabbitMQ 3.13.7 on Erlang/OTP 26 (broker.lock, each by SHA-256; install-broker.sh) at /opt/harness/services/rabbitmq,
# and every image carries ham-services, the helper that starts PostgreSQL and RabbitMQ on 127.0.0.1 inside a session.
RUN --mount=type=bind,source=images/worker-python,target=/img --mount=type=bind,source=images/tools,target=/gate-src \
    if [ "$LANG_FAMILY" = direct ]; then bash /img/install-broker.sh /gate-src/fetch.sh /opt/harness/services/rabbitmq; fi
COPY --chmod=0755 images/worker-python/ham-services.sh /opt/harness/bin/ham-services

# The direct family also carries the in-session container runtime: rootless Podman (containers.lock, each binary by SHA-256) and the
# offline postgres:16 as one flattened layer, staged by images/worker-python/containers-assets.sh before this build. `ham-containers` starts it for
# the engine user (agentd runs `ham-containers up`) behind /var/run/docker.sock; `docker` is Podman's client on that socket. The script is in every
# image (it is small); the runtime and the images exist for the direct family only.
RUN --mount=type=bind,source=build,target=/build --mount=type=bind,source=images/worker-python,target=/img \
    if [ "$LANG_FAMILY" = direct ]; then bash /img/install-containers.sh /build/containers /opt/harness/containers; fi
COPY --chmod=0755 images/worker-python/ham-containers /opt/harness/bin/ham-containers

# `ham-research`: the engine-neutral client of agentd's loopback research API (search, repository download, the read-only context
# directory). A small Python 3.12 program like ham-containers; it holds no key, because there is none in the VM.
COPY --chmod=0755 images/worker-python/ham-research /opt/harness/bin/ham-research

# COMPAT-CAH-DATA: rules the user edited before the rename, and the earlier pack texts, name the helpers `cah-research` and `cah-services`; two links keep those names working until VERSION 6.00.
RUN ln -s ham-research /opt/harness/bin/cah-research && ln -s ham-services /opt/harness/bin/cah-services # COMPAT-CAH-DATA

# `ham-nobody-exec`: starts the engine of an Instances session as the overflow user "nobody" in a private IPC namespace, so programs
# that refuse root run and System V shared memory works without reaching agentd. A small Python 3.12 program; agentd puts it in front of the launcher in the
# `landlock` mode only, so the microVM never runs it.
COPY --chmod=0755 images/worker-python/ham-nobody-exec /opt/harness/bin/ham-nobody-exec

LABEL org.opencontainers.image.revision=$SOURCE_SHA \
      harness.pins.cline=$CLINE_SHA256 harness.pins.opencode=$OPENCODE_SHA256 harness.pins.agentd=$AGENTD_SHA256 \
      harness.family=$LANG_FAMILY

ENV AGENTD_LISTEN=0.0.0.0:8080 \
    AGENTD_PROFILES_DIR=/opt/harness/engine/profiles \
    AGENTD_WORKSPACE_ROOT=/mnt/workspace \
    AGENTD_DATA_ROOT=/var/lib/agentd \
    AGENTD_RUNTIME_DIR=/run/agentd \
    AGENTD_ENGINE_PATH=/opt/harness/bin:/opt/harness/gate-tools/lang/bin:/usr/local/bin:/opt/harness/gate-tools/bin:/usr/bin:/bin \
    AGENTD_ENGINE_USER=agent \
    HARNESS_GATES_TOOLS=/opt/harness/gate-tools \
    AWS_EC2_METADATA_DISABLED=true

EXPOSE 8080
ENTRYPOINT ["/opt/harness/bin/agentd", "serve"]
