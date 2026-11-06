# OpenBikeNet Server

**Status: early draft, implements protocol v0.**

This is the reference server for the [OpenBikeNet protocol](https://github.com/OpenBikeNet/protocol):
a shared, neutral server that lets riders of different indoor cycling apps see each other on
the same route. See the [organisation profile](https://github.com/OpenBikeNet) for the overall goal.

The protocol is designed in the [protocol repository](https://github.com/OpenBikeNet/protocol).
Ideas and feedback are welcome in its Discussions.

## Running it

```sh
git clone --recurse-submodules https://github.com/OpenBikeNet/server
cd server
cargo run -p obn-server
curl -s localhost:8080/v0/stats      # {"apps":[]}
```

Settings come from environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `OBN_BIND` | `0.0.0.0:8080` | Address to listen on |
| `OBN_ROUTES_DIR` | `protocol/routes` | Directory with the route JSON files |
| `OBN_DB` | `obn.sqlite` | SQLite file for the per-app statistics |
| `OBN_TRUST_PROXY` | off | Set to `1` to take the client IP from `X-Forwarded-For` (behind a reverse proxy) |

`RUST_LOG` sets the log level (default `info`). To see riders move, start a fake app that
sends three bots around the velodrome:

```sh
cargo run -p obn-fake-app -- --riders 3
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
