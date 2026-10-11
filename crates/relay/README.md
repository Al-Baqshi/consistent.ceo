# consistent-relay

A rendezvous point so your phone can reach a gateway that only listens on
`127.0.0.1`.

Both sides dial **out**: the gateway holds one outbound WebSocket to the relay,
your phone makes ordinary HTTPS requests to it, and the relay copies bytes
between them. Nothing listens on your home network, no port is forwarded, and
the gateway's `bind = 127.0.0.1` never changes.

## It is a pipe, not an authority

Every forwarded request still carries **your** bearer token, which only your
gateway can validate. A compromised relay can disrupt traffic; it cannot mint
access to your data. The relay adds exactly two rules of its own:

1. A connection must present the registration secret to claim a gateway id, so
   nobody else can impersonate your gateway.
2. Static deploy tokens are refused — reaching a machine remotely requires a
   real signed-in session, mirroring the gateway's own off-loopback guard.

It stores nothing. A restart costs only the reconnect.

## Run it

```bash
# Generate a secret once; both the relay and your gateway use it.
export CONSISTENT_RELAY_SECRET=$(openssl rand -hex 32)

# Locally, or anywhere that can hold a WebSocket (a small VM, Fly, Railway):
cargo run --release -p consistent-world-relay
# or
docker build -f crates/relay/Dockerfile -t consistent-relay .
docker run -e CONSISTENT_RELAY_SECRET=$CONSISTENT_RELAY_SECRET -p 8787:8787 consistent-relay
```

Put it behind TLS — a reverse proxy, or a platform that terminates for you.
Use `wss://` from the gateway once you do.

## Point your gateway at it

```bash
export CONSISTENT_WORLD_RELAY_URL=wss://relay.example.com
export CONSISTENT_WORLD_RELAY_SECRET=$CONSISTENT_RELAY_SECRET
export CONSISTENT_WORLD_RELAY_GATEWAY_ID=my-mac        # any stable id you pick
consistent-world start
```

Then `consistent-world pair` shows a QR pointing at
`https://relay.example.com/g/my-mac`, which works from any network. Relay,
tailnet, and LAN are tried in that order, so adding a relay simply upgrades
what pairing hands out.

## Endpoints

| Route | Who calls it |
|---|---|
| `GET /health` | you, or your platform's health check |
| `GET /link` | the gateway's outbound WebSocket (one per gateway id) |
| `/g/{gateway_id}/*` | the phone — forwarded to that gateway |

A request for a gateway that is not connected returns **503**, which the app
renders as "your Mac is offline" rather than a generic failure.
