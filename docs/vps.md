# Running the bot on a small server near the chain

## Why

Every loss the bot has refused so far was a price that had already moved. On
2026-09-24, 434 of 443 refusals had a leg that moved more than a basis point between
detection and the fresh read a round trip later. The data reaches a home PC in India
late, and each of the three calls an attempt makes costs about 100 ms more.

A server in the same region as the RPC provider and the Jito block engine cuts each of
those calls to a few milliseconds. It also cuts the websocket feed's delay by the same
amount. Code cannot remove that distance, and it is the cheapest lever left: about
$5–10 a month, against $450+ a month for a shred feed.

This does not guarantee profit. It moves the bot from far behind the competition to
merely behind it. Other bots on this market sit inside the validators.

## What it costs and risks

- **Money:** a 1–2 vCPU, 2 GB VM, about $5–10 a month (Hetzner, Vultr, DigitalOcean,
  OVH). The bot uses little CPU; memory matters more than cores.
- **Key:** the encrypted keypair file goes on the server. It stays encrypted there,
  and the passphrase is typed into the running process, never written to disk.
  Anyone with root on the server could still read memory while the bot runs, so use
  a provider you trust and an SSH key, not a password.
- **Nothing else changes:** Jito, the floors, the risk limits and the $1 loss budget
  all behave the same.

## Pick the region

Run the probe from a candidate server before settling on it. The lowest total for
Helius plus the Jito block engine wins; Frankfurt or Amsterdam usually does in Europe,
New York in North America.

```bash
bash scripts/latency-probe.sh
```

Then set the matching block engine in `config.toml`, for example:

```toml
jito_url = "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/transactions?bundleOnly=true"
```

## Set it up (Ubuntu 24.04)

On the server:

```bash
git clone https://github.com/<you>/cryptobot.git && cd cryptobot
bash scripts/vps-setup.sh
```

From your PC, copy the two private files. Neither is in git:

```bash
scp config.toml keypair-encrypted.json <user>@<server>:cryptobot/
```

Start it inside `tmux` so it keeps running when you disconnect:

```bash
tmux new -s bot
CRYPTOBOT_ALLOW_LIVE=1 ./target/release/cb-bot
# type the wallet passphrase when asked; detach with Ctrl-b then d
```

Stop the bot on your PC first. Two bots signing for one wallet would compete with
each other for the same trades.

## Watch it from the desk

The bot's API listens on `127.0.0.1:8787` on the server only; it is never exposed.
Forward it over SSH and the desk's Live view shows the server's bot:

```bash
ssh -N -L 8787:127.0.0.1:8787 <user>@<server>
```

The desk's Start and Stop buttons still control the local bot, so leave the local one
stopped. To stop the server's bot, reattach with `tmux attach -t bot` and press
Ctrl-C.
