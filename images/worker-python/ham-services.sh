#!/usr/bin/env bash
# ham-services: starts the data services a sample project's tests need inside a direct-run session, on 127.0.0.1 only, as
# the calling user (the engine, never root), with the settings the three sample prompts give their Compose services:
#   ham-services postgres   PostgreSQL 16 on 127.0.0.1:5432, user app, database app, trust authentication (no password), TCP only: it opens no
#                           Unix-domain socket (a session's temp directory is deeper than the 107 bytes a socket path may hold), so a client names
#                           the host, as DATABASE_URL does (psql -h 127.0.0.1)
#   ham-services rabbitmq   RabbitMQ 3.13 on 127.0.0.1:5672, user app, a per-run random password (BROKER_PASSWORD when set)
#   ham-services env        the export lines for DATABASE_URL, BROKER_PASSWORD and BROKER_URL of what is running (eval "$(ham-services env)")
#   ham-services status     one line per service: running or stopped
#   ham-services stop       stops both (PostgreSQL by a fast shutdown, so a connection a test left open does not keep it running)
# The session has no Docker; these are the image's own native servers (/usr/bin/postgres, /opt/harness/services/rabbitmq).
# Data lives under $HAMH_SERVICES_DIR (default $TMPDIR/ham-services, the engine's local temp directory) and goes with the session. No password is
# stored in a repository or an image: the broker's is made here from /dev/urandom and kept in a 0600 file of that directory. Starting a
# running service again does nothing. Ends 0 when what was asked is ready, 1 with a reason when it is not (a server that exits right after it
# starts is reported at once, with the end of its log, not after the full wait). HAMH_SERVICES_PG_PORT, a test knob, moves PostgreSQL to another port.
set -euo pipefail
S="${HAMH_SERVICES_DIR:-${TMPDIR:-/tmp}/ham-services}"
RMQ=/opt/harness/services/rabbitmq
PG_PORT=5432 AMQP_PORT=5672
say() { echo "ham-services: $*"; }
die() { echo "ham-services: $*" >&2; exit 1; }
# A test knob: another port for PostgreSQL, so a test can run this script beside a real server. The sample prompts, the report and the
# smoke check all use 5432 and set nothing.
if [ -n "${HAMH_SERVICES_PG_PORT:-}" ]; then
  case $HAMH_SERVICES_PG_PORT in ''|*[!0-9]*|??????*) die "HAMH_SERVICES_PG_PORT must be a port number from 1 to 65535" ;; esac
  [ "$HAMH_SERVICES_PG_PORT" -ge 1 ] && [ "$HAMH_SERVICES_PG_PORT" -le 65535 ] || die "HAMH_SERVICES_PG_PORT must be a port number from 1 to 65535"
  PG_PORT=$HAMH_SERVICES_PG_PORT
fi
[ "$(id -u)" != 0 ] || die "run it as the engine user, not root (PostgreSQL refuses root)"
mkdir -p "$S"
chmod 0700 "$S"

listening() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
ended() { # <label> <log>: the server process is gone and its port never opened; print the end of its log, then one short last line with the reason
  local why
  tail -n 5 "$2" >&2 2>/dev/null || true
  why=$(grep -E 'FATAL|PANIC' "$2" 2>/dev/null | tail -n 1 | sed -E 's/^.*(FATAL|PANIC): +//') || why=
  [ -n "$why" ] || why=$(grep -v '^[[:space:]]*$' "$2" 2>/dev/null | tail -n 1) || why=
  die "$1 exited right after it started (${why:-nothing in its log}); see $2"
}
wait_for() { # <port> <seconds> <label> <log> [<pid of the server: the wait ends at once when it is gone>]
  local i
  for i in $(seq 1 "$2"); do
    listening "$1" && return 0
    if [ -n "${5:-}" ] && ! kill -0 "$5" 2>/dev/null; then ended "$3" "$4"; fi
    sleep 1
  done
  die "$3 did not accept connections on 127.0.0.1:$1 within $2 seconds; see $4"
}

start_postgres() {
  local d=$S/postgres
  if listening "$PG_PORT"; then say "postgres already listening on 127.0.0.1:$PG_PORT"; return 0; fi
  mkdir -p "$d"
  if [ ! -f "$d/data/PG_VERSION" ]; then
    initdb -D "$d/data" -U app -A trust --encoding=UTF8 --locale=C --no-sync >"$d/initdb.log" 2>&1 || die "initdb failed; see $d/initdb.log"
  fi
  # No Unix-domain socket: a socket path holds 107 bytes at most and this directory is deeper than that in a live session, so the
  # server stopped at once with "Unix-domain socket path... is too long". Clients use TCP. A non-interactive shell makes the background
  # command no process group leader, so setsid does not fork and $! is the server itself.
  setsid nohup postgres -D "$d/data" -c listen_addresses=127.0.0.1 -c "port=$PG_PORT" -c "unix_socket_directories=" \
    -c fsync=off -c synchronous_commit=off -c full_page_writes=off >"$d/postgres.log" 2>&1 &
  echo $! >"$d/postgres.pid"
  wait_for "$PG_PORT" 60 postgres "$d/postgres.log" "$(cat "$d/postgres.pid")"
  psql -h 127.0.0.1 -p "$PG_PORT" -U app -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname = 'app'" | grep -q 1 ||
    psql -h 127.0.0.1 -p "$PG_PORT" -U app -d postgres -qc 'CREATE DATABASE app' || die "could not create the database app"
  say "postgres ready: postgresql://app@127.0.0.1:$PG_PORT/app (trust authentication, no password)"
}

rabbit_env() { # the environment every RabbitMQ and Erlang command of this broker needs: one node, loopback only
  local d=$S/rabbitmq
  export RABBITMQ_MNESIA_BASE="$d/mnesia" RABBITMQ_LOG_BASE="$d/log" RABBITMQ_CONFIG_FILE="$d/rabbitmq" RABBITMQ_NODENAME=rabbit@localhost
  export RABBITMQ_NODE_IP_ADDRESS=127.0.0.1 RABBITMQ_NODE_PORT="$AMQP_PORT" RABBITMQ_DIST_PORT=25672 ERL_EPMD_ADDRESS=127.0.0.1
  export RABBITMQ_SERVER_ADDITIONAL_ERL_ARGS="-kernel inet_dist_use_interface {127,0,0,1}"
  export RABBITMQ_CTL_ERL_ARGS="-kernel inet_dist_use_interface {127,0,0,1}"
}

start_rabbitmq() {
  local d=$S/rabbitmq pw
  [ -x "$RMQ/sbin/rabbitmq-server" ] || die "RabbitMQ is not in this image ($RMQ)"
  if listening "$AMQP_PORT"; then say "rabbitmq already listening on 127.0.0.1:$AMQP_PORT"; return 0; fi
  mkdir -p "$d/mnesia" "$d/log"
  chmod 0700 "$d"
  if [ -n "${BROKER_PASSWORD:-}" ]; then pw=$BROKER_PASSWORD
  elif [ -s "$d/broker-password" ]; then pw=$(cat "$d/broker-password")
  else pw=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'); fi
  (umask 077 && printf '%s' "$pw" >"$d/broker-password")
  (umask 077 && printf 'default_user = app\ndefault_pass = %s\nloopback_users = none\n' "$pw" >"$d/rabbitmq.conf")
  rabbit_env
  setsid nohup "$RMQ/sbin/rabbitmq-server" >"$d/rabbitmq.out" 2>&1 &
  echo $! >"$d/rabbitmq.pid"
  wait_for "$AMQP_PORT" 120 rabbitmq "$d/rabbitmq.out"
  "$RMQ/sbin/rabbitmqctl" -q await_startup >/dev/null 2>&1 || true
  say "rabbitmq ready: amqp://app:<BROKER_PASSWORD>@127.0.0.1:$AMQP_PORT/ (password in $d/broker-password; eval \"\$(ham-services env)\")"
}

env_lines() {
  listening "$PG_PORT" && echo "export DATABASE_URL='postgres://app@127.0.0.1:$PG_PORT/app?sslmode=disable'"
  if [ -s "$S/rabbitmq/broker-password" ]; then
    echo "export BROKER_PASSWORD='$(cat "$S/rabbitmq/broker-password")'"
    echo "export BROKER_URL=\"amqp://app:\${BROKER_PASSWORD}@127.0.0.1:$AMQP_PORT/\""
  fi
  return 0
}

stop() {
  local pid="" i data=$S/postgres/data/postmaster.pid
  rabbit_env
  # PostgreSQL gets an interrupt, a fast shutdown: a terminate waits for every open connection, and a pool a test left open would keep the
  # port busy for the next start. It starts now and is waited for after RabbitMQ stopped, within the time the report allows the helper.
  if [ -f "$S/postgres/postgres.pid" ]; then pid=$(cat "$S/postgres/postgres.pid"); kill -INT "$pid" 2>/dev/null || true; fi
  [ -x "$RMQ/sbin/rabbitmqctl" ] && "$RMQ/sbin/rabbitmqctl" -q stop >/dev/null 2>&1 || true
  if [ -f "$S/rabbitmq/rabbitmq.pid" ]; then kill "$(cat "$S/rabbitmq/rabbitmq.pid")" 2>/dev/null || true; fi
  if [ -n "$pid" ]; then
    # The server removes postmaster.pid when it has shut down; a pid that is gone ends the wait too.
    for i in $(seq 1 50); do
      [ -f "$data" ] && kill -0 "$pid" 2>/dev/null || break
      sleep 0.2
    done
    if [ -f "$data" ] && kill -0 "$pid" 2>/dev/null; then kill -QUIT "$pid" 2>/dev/null || true; fi
  fi
  say "stopped"
}

status() {
  listening "$PG_PORT" && say "postgres running on 127.0.0.1:$PG_PORT" || say "postgres stopped"
  listening "$AMQP_PORT" && say "rabbitmq running on 127.0.0.1:$AMQP_PORT" || say "rabbitmq stopped"
}

case "${1:-}" in
  postgres) start_postgres ;;
  rabbitmq) start_rabbitmq ;;
  env) env_lines ;;
  status) status ;;
  stop) stop ;;
  *) echo "usage: ham-services postgres | rabbitmq | env | status | stop" >&2; exit 2 ;;
esac
