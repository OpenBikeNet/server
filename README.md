# OpenBikeNet Server

**Status: early draft, implements protocol v0.**

This is the reference server for the [OpenBikeNet protocol](https://github.com/OpenBikeNet/protocol):
a shared, neutral server that lets riders of different indoor cycling apps see each other on
the same route. See the [organisation profile](https://github.com/OpenBikeNet) for the overall goal.

The protocol is designed in the [protocol repository](https://github.com/OpenBikeNet/protocol).
Ideas and feedback are welcome in its Discussions.

## Public server

A public instance runs at [openbikenet.cloud](https://openbikenet.cloud). Apps connect to
`wss://openbikenet.cloud/v0`, and `https://openbikenet.cloud/v0/stats` shows which apps are on.

## Running it

With Docker (nothing else needed):

```sh
git clone --recurse-submodules https://github.com/OpenBikeNet/server
cd server
make up
curl -s localhost:8090/v0/stats      # {"apps":[]}
```

`make up` creates `.env`, builds the image and waits until the server is healthy. It listens on
port 8090 of this machine only, so it can run next to other servers. Run `make` to see all targets:

| Target | What it does |
| --- | --- |
| `make up` | Build and start, then wait until healthy |
| `make down`, `make restart` | Stop or restart (asks first if riders are on) |
| `make logs`, `make ps`, `make health` | Look at the running server |
| `make update` | `git pull`, update the submodule, rebuild, restart |
| `make dev` | Run with cargo on `127.0.0.1:8090` (reads `.env.dev`) |
| `make fake ARGS="--riders 3"` | Start fake riders against the server |
| `make check` | What CI runs: format, clippy, tests |

Settings are in `.env` (see `.env.example`): the host address and port, `OBN_TRUST_PROXY` and
`RUST_LOG`. `make update` adds settings that are new in `.env.example` and keeps your values.
If riders are on the server, `make update`, `make restart` and `make down` ask first. `FORCE=1`
skips the question. The usage statistics live in the `data` volume.

Without Docker, run it with cargo:

```sh
cargo run -p obn-server
curl -s localhost:8080/v0/stats      # {"apps":[]}
```

The server reads these environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OBN_BIND` | `0.0.0.0:8080` | Address to listen on |
| `OBN_ROUTES_DIR` | `protocol/routes` | Directory with the route JSON files |
| `OBN_DB` | `obn.sqlite` | SQLite file for the per-app statistics |
| `OBN_TRUST_PROXY` | off | Set to `1` (or `true`) to take the client IP from `X-Forwarded-For` (behind a reverse proxy) |

`RUST_LOG` sets the log level (default `info`). To see riders move, start a fake app that
sends three bots around the velodrome:

```sh
make fake ARGS="--riders 3"
```

## Monitoring

`GET /internal/metrics` returns Prometheus text. It holds only aggregate counts per app ID
(already public via `/v0/stats`), never rider names, rider values or IPs. The bundled Caddy
answers 404 on `/internal/*`, so it is not public. `GET /healthz` returns `ok`.

| Series | Type | Meaning |
| --- | --- | --- |
| `obn_connections{app}` | gauge | Connections per live app |
| `obn_riders{app}` | gauge | Riders per live app |
| `obn_connections_total` | counter | WebSocket connections accepted |
| `obn_rider_updates_total` | counter | Accepted update entries |
| `obn_errors_total{code}` | counter | Errors sent, one series per error code |
| `obn_queue_full_drops_total` | counter | Connections dropped because they read too slowly |
| `obn_connections_rejected_total{reason="per_ip_limit"}` | counter | Connections refused by the per-IP limit |
| `obn_build_info{version,sha}` | gauge | Always 1; says which build runs |

The server joins the Docker network `openbikenet-metrics` with the alias `obn-server`. Put your
Prometheus on that network and scrape it there:

```yaml
scrape_configs:
  - job_name: openbikenet
    metrics_path: /internal/metrics
    static_configs:
      - targets: ["obn-server:8080"]
```

## HTTPS

Apps in browsers need `wss://`. There are two ways:

- **Bundled Caddy.** Point a DNS record at the server, then set `COMPOSE_PROFILES=tls`,
  `OBN_DOMAIN=net.example.org` and `OBN_TRUST_PROXY=1` in `.env` and run `make up`. Caddy gets
  and renews the certificate itself.
- **Your own reverse proxy.** Proxy to port 8090, allow WebSockets, block `/internal/*`
  and set `OBN_TRUST_PROXY=1`. In Caddy:

  ```
  net.example.org {
  	handle /internal/* {
  		respond 404
  	}
  	handle {
  		reverse_proxy localhost:8090
  	}
  }
  ```

## Goals

- **Shared presence first:** apps report their riders' positions on shared routes and receive
  the positions of riders from other apps. Each app keeps its own physics.
- **Self-hostable:** anyone can run their own instance. A public instance is planned as a
  community service.
- **Minimal data:** only what riding together needs, nothing stored that isn't required.

## License

[GNU Affero General Public License v3.0](LICENSE). If you run a modified version as a public
service, you share your changes. Apps that only talk to the server over the protocol are not
affected and can use any license.
