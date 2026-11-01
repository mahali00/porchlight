# Tunnels

MCP clients reach porchlight through a public HTTPS address that forwards to one local port. That port only serves OAuth and the `/<link>/mcp` endpoints. The approval page listens on a separate port that no tunnel reaches.

```sh
porchlight tunnel        # current tunnel, public address and local port
```

## OpenTunnel (default)

[OpenTunnel](https://github.com/anomalyco/opentunnel) needs no account. It gives this computer a stable `porchlight.<id>.opentunnel.xyz` address. TLS ends on your computer, so the relay doesn't see the traffic. `porchlight start` installs OpenTunnel 0.5.0 with npm or bun if it's missing, and says so.

```sh
porchlight tunnel use opentunnel
```

## Your own tunnel

Point any HTTPS tunnel or reverse proxy at `http://127.0.0.1:<port>`, using the port shown by `porchlight tunnel`. Then tell porchlight its public address:

```sh
porchlight tunnel use https://mcp.example.com
```

The port stays the same between runs. To choose it yourself, set `"port"` in `porchlight.json`.

### cloudflared with your domain

A named Cloudflare Tunnel on a domain you have on Cloudflare:

```sh
cloudflared tunnel login
cloudflared tunnel create porchlight
cloudflared tunnel route dns porchlight mcp.example.com
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: porchlight
credentials-file: /home/you/.cloudflared/<tunnel-id>.json
ingress:
  - hostname: mcp.example.com
    service: http://127.0.0.1:18080
  - service: http_status:404
```

Use the port from `porchlight tunnel` instead of `18080`. Then:

```sh
cloudflared service install
porchlight tunnel use https://mcp.example.com
```

Cloudflare's free Quick Tunnels (`cloudflared tunnel --url …`) don't work with porchlight. They don't support server-sent events, which MCP uses, and their address changes on every run, which makes every client approve again ([Cloudflare docs](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/)).

## Changing tunnels

Clients keep their sign-in per address. Moving to a new public address means clients add the new link address and approve again.
