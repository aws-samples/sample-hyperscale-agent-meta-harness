Build a new n-tier order-events system in Go 1.27 in this empty repository, test-first. Tiers: an
HTTP service (`cmd/api`) that accepts `POST /orders` (JSON with item and quantity), stores the order
in PostgreSQL with status `pending` and publishes an `order.created` message to RabbitMQ, serves `GET
/orders/{id}` and `GET /healthz`; and a queue-driven worker (`cmd/worker`) that consumes
`order.created`, marks the order `processed` in PostgreSQL and is idempotent when a message is
delivered twice. Put a `compose.yaml` at the repository root with services `db` (image
`postgres:16`, `POSTGRES_USER=app`, `POSTGRES_DB=app` and `POSTGRES_HOST_AUTH_METHOD=trust`, with no
password of any kind in the repository, a `pg_isready -U app` healthcheck, port
`127.0.0.1:5432:5432`) and `broker` (image `rabbitmq:3.13`, environment `RABBITMQ_DEFAULT_USER=app`
and `RABBITMQ_DEFAULT_PASS=${BROKER_PASSWORD:?set BROKER_PASSWORD}`, so the broker's password is a
random per-run value that the caller puts in the environment and the repository never holds a
password value, a `rabbitmq-diagnostics -q ping` healthcheck, port `127.0.0.1:5672:5672`). Both
binaries read `DATABASE_URL` (default `postgres://app@127.0.0.1:5432/app?sslmode=disable`) and
`BROKER_URL` (default `amqp://app@127.0.0.1:5672/`; the run-time value of `BROKER_URL` carries the
broker's per-run password). Tests: unit tests per tier with `go test` (the HTTP handlers and
the worker's message handling against in-memory fakes); integration tests (build tag `integration`)
of the store against the real PostgreSQL and of publish and consume against the real RabbitMQ from
`compose.yaml`; and at least one end-to-end test (build tag `e2e`) that runs the API and the worker
in-process against both, posts an order through the public HTTP API and polls `GET /orders/{id}`
until it is `processed`. Add these scripts, each run as `bash <script>` from the repository root,
each exiting non-zero on any failure and writing JUnit XML with `go run
gotest.tools/gotestsum@v1.13.0 --junitfile`: `scripts/test-unit.sh` (JUnit XML to
`test-results/unit/`), `scripts/test-integration.sh` (JUnit XML to `test-results/integration/`) and
`scripts/test-e2e.sh` (JUnit XML to `test-results/e2e/`). A clean room runs them in `golang:1.27` with
the `compose.yaml` services running and `DATABASE_URL` and `BROKER_URL` pointing at them.
