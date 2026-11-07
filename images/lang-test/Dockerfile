# The language test container (host architecture): the worker image's
# Amazon Linux 2023 base with git, gcc (cgo for `go test -race`) and the archive tools the lang
# installer needs. Node 24 needs glibc 2.28 and an older Linux machine has 2.26, so the tests that run the
# language tool set (agentd's lock step, crates/agentd/tests/lock_turn.rs) run here; the tool sets are mounted
# from HAMH_TOOLS (scripts/gates-lang-tools.sh). The native services install as in the
# worker image (images/services/install.sh, the named build context `services`),
# plus the MariaDB client the retired services tests called (a trim candidate with the MariaDB server of the worker image).
#checkov:skip=CKV_DOCKER_2:a short-lived build and test container started by docker run with the rm option for one command by scripts/gates-lang-tools.sh, it is not a service, so nothing probes its health
#checkov:skip=CKV_DOCKER_3:its only runner, scripts/gates-lang-tools.sh, starts it with the user option set to the host uid and gid, so files it writes into the mounted directories belong to the host user and an image user would not match that id
FROM public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:381352c430f36d8aa6b8b670de06e7e4e5642fe0cb9dbd463bb8d1e79aefc0c7
RUN dnf install -y --setopt=install_weak_deps=False \
      git util-linux shadow-utils findutils procps-ng tar gzip xz unzip which gcc glibc-devel \
    && dnf clean all && rm -rf /var/cache/dnf
# nosemgrep: missing-dnf-clean-all, nonsensical-command -- the same RUN removes the downloaded rpm and the dnf cache itself, so no cache stays in the layer, and the mount option of RUN is the BuildKit flag that binds the named build context into this step and is not the mount command, the rule pattern reads the word mount inside the option
RUN --mount=type=bind,from=services,target=/svc bash /svc/install.sh \
    && mkdir -p /tmp/c && dnf install -y -q --downloadonly --downloaddir /tmp/c mariadb114-11.4.13-1.amzn2023.0.2 >/dev/null \
    && rpm -i --nodeps --noscripts --excludedocs /tmp/c/mariadb114-11.4.13-*.rpm \
    && rm -rf /tmp/c /var/cache/dnf && mariadb --version && psql --version
