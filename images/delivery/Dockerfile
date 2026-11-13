# Delivery image, linux/arm64: one image for the three
# container-image Lambdas repo-gateway, bundle-validator and packager (crates/delivery), plus git
# and gitleaks 8.30.1. Built only by images/codebuild/delivery.yml on CodeBuild ARM, which first
# places under build/ the three bootstraps (cargo lambda arm64 builds from scripts/deploy.sh) and
# the pinned gitleaks release binary. Every binary is checked against its SHA-256 here. Each
# function's ImageConfig.EntryPoint names its binary; the Rust binaries speak the Lambda Runtime
# API themselves, so no runtime interface client is needed.
# checkov:skip=CKV_DOCKER_2:a Lambda container image, Lambda starts and replaces its execution environments itself and the image runs no listening service for a HEALTHCHECK to probe
# checkov:skip=CKV_DOCKER_3:Lambda starts a container image as its own least-privileged default Linux user, so the image needs no USER instruction, as the AWS Lambda developer guide describes for container images
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:381352c430f36d8aa6b8b670de06e7e4e5642fe0cb9dbd463bb8d1e79aefc0c7

ARG REPO_GATEWAY_SHA256
ARG BUNDLE_VALIDATOR_SHA256
ARG PACKAGER_SHA256
ARG GITLEAKS_SHA256
ARG SOURCE_SHA=unknown

# git-core: git with its HTTPS remote helper; no perl, no GUI tools.
RUN dnf install -y --setopt=install_weak_deps=False git-core \
    && dnf clean all && rm -rf /var/cache/dnf

COPY build/repo-gateway build/bundle-validator build/packager build/gitleaks /opt/delivery/bin/

# nosemgrep: set-pipefail, use-workdir -- printf cannot fail here and sha256sum with the check option exits non-zero on any mismatch, so the pipeline status is the checksum verdict, and the cd only scopes the checksum check and the chmod of this one RUN, later instructions use absolute paths and the ENTRYPOINT is /opt/delivery/bin/repo-gateway
RUN cd /opt/delivery/bin \
    && test -n "$REPO_GATEWAY_SHA256" && test -n "$BUNDLE_VALIDATOR_SHA256" && test -n "$PACKAGER_SHA256" && test -n "$GITLEAKS_SHA256" \
    && printf '%s  repo-gateway\n%s  bundle-validator\n%s  packager\n%s  gitleaks\n' \
         "$REPO_GATEWAY_SHA256" "$BUNDLE_VALIDATOR_SHA256" "$PACKAGER_SHA256" "$GITLEAKS_SHA256" | sha256sum -c - \
    && chmod 0755 repo-gateway bundle-validator packager gitleaks \
    && printf '{"repo-gateway":"%s","bundle-validator":"%s","packager":"%s","gitleaks":"%s","source":"%s"}\n' \
         "$REPO_GATEWAY_SHA256" "$BUNDLE_VALIDATOR_SHA256" "$PACKAGER_SHA256" "$GITLEAKS_SHA256" "$SOURCE_SHA" > /opt/delivery/pins.json

LABEL org.opencontainers.image.revision=$SOURCE_SHA \
      harness.pins.repo-gateway=$REPO_GATEWAY_SHA256 harness.pins.bundle-validator=$BUNDLE_VALIDATOR_SHA256 \
      harness.pins.packager=$PACKAGER_SHA256 harness.pins.gitleaks=$GITLEAKS_SHA256

# Lambda mounts only /tmp writable; the functions keep scratch, HOME and bundles under WORK_ROOT.
ENV GIT_BIN=/usr/bin/git \
    GITLEAKS_BIN=/opt/delivery/bin/gitleaks \
    WORK_ROOT=/tmp/delivery

# nosemgrep: missing-user-entrypoint -- Lambda starts a container image as its own least-privileged default Linux user, so the image needs no USER instruction, as the AWS Lambda developer guide describes for container images
ENTRYPOINT ["/opt/delivery/bin/repo-gateway"]
