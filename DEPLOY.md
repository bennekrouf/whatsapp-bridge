# Deploying the bridge

This service serves two doors from one binary: the WhatsApp webhook
(`/webhook/{tenant_id}`) and the Telegram webhook (`/telegram/webhook/{bot_id}`).

## How it starts in production

pm2 does **not** run the binary directly, and there is no `ecosystem.config.js`
in this repo on purpose — one existed briefly and described a startup model the
host does not use, which is worse than having none.

```
pm2 process  api0-whatsapp-bridge
  └─ runs    /opt/api0/run-whatsapp-bridge.sh     (cwd: /home/ubuntu)
       ├─ sources /opt/api0/whatsapp-bridge.env   (secrets + CONFIG_PATH)
       └─ execs   /opt/api0/bin/whatsapp-bridge
```

Both are version-controlled here:

- `deploy/run-whatsapp-bridge.sh` — the wrapper. The host's copy at
  `/opt/api0/run-whatsapp-bridge.sh` should be a **symlink** to it, so the file
  in git keeps describing what actually runs.
- `deploy/whatsapp-bridge.env.example` — the shape of the env file. The real
  `/opt/api0/whatsapp-bridge.env` holds secrets and is not in git.

`whatsapp-bridge.env` holds `CLAUDE_API_KEY`, `API0_INTERNAL_SECRET`, optionally
`META_APP_SECRET`, plus `LOG_PATH_API0` and `CONFIG_PATH`.

WhatsApp webhooks are checked against the tenant's own App Secret (saved with
its channel), falling back to `META_APP_SECRET`. A tenant with neither has its
messages refused with 401 — the dashboard's WhatsApp **Test** says so. The
`ALLOW_UNSIGNED_WEBHOOKS=true` override accepts them unchecked and exists for
local development only; the bridge logs an error at startup when it is on.

`CONFIG_PATH` must point at **`config_production.yaml`**, not `config.yaml`.

Both point the bridge at the gateway on **`127.0.0.1:5009`** — the port the
gateway actually listens on, and the one nginx proxies `gateway.api0.ai` to.
PM2 sets `API0__SERVER__PORT=50054` for the gateway, but that override has no
effect; a bridge pointed at 50054 links accounts fine and then fails every
message with "Gateway unreachable". To check on the host:

```
sudo ss -ltnp | grep gateway            # the gateway's real port
pm2 logs api0-whatsapp-bridge | grep Gateway:
```

The bridge logs the address at startup:

```
Config: /opt/api0/src/whatsapp-bridge/config_production.yaml
Gateway: http://127.0.0.1:5009
```

## Updating

`deploy/update.sh` in the gateway repo handles this: it pulls, runs
`cargo build --release`, copies the binary to `/opt/api0/bin/whatsapp-bridge`
(what the wrapper execs) and restarts `api0-whatsapp-bridge`.

The copy is the step to remember when doing it by hand — building alone changes
nothing, because the wrapper never looks at `target/release/`:

```bash
cd /opt/api0/src/whatsapp-bridge && git pull && cargo build --release
cp target/release/whatsapp-bridge /opt/api0/bin/whatsapp-bridge.new
chmod 755 /opt/api0/bin/whatsapp-bridge.new
mv /opt/api0/bin/whatsapp-bridge.new /opt/api0/bin/whatsapp-bridge
pm2 restart api0-whatsapp-bridge
```

`cp`→`chmod`→`mv` rather than writing in place: the running process holds the
file open, and `mv` swaps the directory entry atomically.

Restart with a plain `pm2 restart`. Never `--update-env` — the process env comes
from the wrapper's env file, and `--update-env` replaces it with the invoking
shell's, which will not have `CLAUDE_API_KEY`.

## Verifying

```bash
curl -s -o /dev/null -w '%{http_code}\n' \
  -X POST https://wa-bridge.api0.ai/telegram/webhook/0 \
  -H 'Content-Type: application/json' -d '{}'
```

`200` means the Telegram route is live (bot `0` is unknown, and the handler
answers 200 to everything so Telegram does not retry forever). `404` means the
running binary predates Telegram support — the copy step above was missed.
