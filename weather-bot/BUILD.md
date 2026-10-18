# BUILD.md

## 0. Dashboard, HTTPS and accounts
`sudo bash scripts/install.sh` builds the bot, asks for an admin username and password, and sets up HTTPS with Caddy.
- **No browser warning:** Caddy obtains and renews a trusted Let's Encrypt certificate by itself. With no domain it uses a
  free automatic hostname built from your IP (for example `64-94-85-17.sslip.io`); with your own domain, point its A record
  at the server first. Ports 80 and 443 must be open in your VPS provider's firewall.
- Only an Argon2 hash of the password is stored (`/etc/weather-bot/admin.json`, 0600). The page is read-only.
- Change the login: `sudo WEATHER_BOT_CONFIG_DIR=/etc/weather-bot/config weather-bot set-admin` then restart the service.
- **Two accounts, same signals.** *Paper* always runs (simulated fills on real order books). *Live* is added when you start
  with `weather-bot live --enable`. Each has its own tables (`paper_orders`, `live_orders`, ...), positions, exposure,
  kill switch and daily loss cap, but both act on the same signals, edge and probabilities. Switch between them at the top of the dashboard.
- **Upgrading:** unzip the new version with `unzip` (it keeps hidden files) and run the installer again. Config, secrets and
  the database are kept; an old single-account database is migrated automatically (its tables become `paper_*`).
- **Kill switch:** when an account's drawdown reaches 15% it stops for good and writes `/var/lib/weather-bot/KILL_SWITCH_paper`
  (or `_live`). The bot keeps running so you can see why on the dashboard. To restart *paper* from a clean $1,000 (old data
  is archived, calibration data is kept): `sudo systemctl stop underhaven-weather`, then
  `sudo -E bash -c 'set -a; . /etc/weather-bot/env; set +a; weather-bot reset-paper --yes'`, then start the service.
  For *live*, review what happened, then delete the `KILL_SWITCH_live` file and restart.
- **Daily loss cap** (default 5% per account, resets at 00:00 UTC) and **retry limit** (3 unfilled attempts per market per day,
  15 minutes apart) are in `risk.toml`.

## 1. Build on a fresh Ubuntu 22.04 / 24.04 VPS
```bash
sudo apt update && sudo apt install -y unzip
unzip weather-bot.zip && cd weather-bot
sudo bash scripts/install.sh    # apt deps, rustup, cargo build --release, install, systemd enable --now
```
Manual equivalent:
```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev sqlite3 curl
curl https://sh.rustup.rs -sSf | sh -s -- -y && . "$HOME/.cargo/env"
cargo build --release          # fix any compile errors first: this code has not been compiled by its author
cargo test
```
Installed paths: binary `/usr/local/bin/weather-bot`, config `/etc/weather-bot/config/`, secrets
`/etc/weather-bot/env` (chmod 600), data `/var/lib/weather-bot/`, unit `underhaven-weather.service`.

```bash
systemctl status underhaven-weather
journalctl -u underhaven-weather -f
weather-bot stations                     # needs the env: set -a; . /etc/weather-bot/env; set +a
weather-bot signals --city NYC           # evaluate only, no orders
weather-bot rebates
weather-bot export --csv                 # to $WEATHER_BOT_EXPORT_DIR
```
The service runs `dry-run` (paper). Dry-run uses real books and simulated fills.

## 2. Switch from dry-run to live
Do not do this until dry-run has run for days and you've inspected `signals`, `orders` and `events`.
1. Put real values in `/etc/weather-bot/env`: `POLYMARKET_PRIVATE_KEY`, `POLYMARKET_FUNDER_ADDRESS`, and
   `POLYMARKET_SIGNATURE_TYPE` (0 EOA, 1 proxy, 2 Safe). Set `bankroll_usdc` in `risk.toml` to what you actually fund.
2. Start with a tiny bankroll to verify order signing against the real CLOB.
3. Set `live_bankroll_usdc` in `/etc/weather-bot/config/risk.toml` to the USDC you actually fund (live refuses to start at 0).
4. Override the unit's command (this runs BOTH accounts: paper keeps going alongside live):
```bash
sudo systemctl edit underhaven-weather
# add:
# [Service]
# ExecStart=
# ExecStart=/usr/local/bin/weather-bot live --enable
sudo systemctl restart underhaven-weather
```
Live refuses to start without `--enable` AND both env keys (exit code 2). Add `--allow-taker` only if you want
taker fallbacks. Back to paper: remove the override (`sudo systemctl revert underhaven-weather`).

## 3. Read the SQLite tables
```bash
sqlite3 -header -column /var/lib/weather-bot/weather-bot.db
sqlite> SELECT ts,city,strategy,bracket,model_prob,market_ask,edge,settlement_check,decision,reason FROM signals ORDER BY id DESC LIMIT 20;
sqlite> SELECT ts,signal_id,side,price,size,post_only,status,filled,fee FROM orders ORDER BY id DESC LIMIT 20;
sqlite> SELECT * FROM fills ORDER BY id DESC LIMIT 20;
sqlite> SELECT city,bracket,side,size,avg_price,status,pnl FROM positions;
sqlite> SELECT ts,level,kind,message FROM events ORDER BY id DESC LIMIT 30;   -- SETTLEMENT_UNSAFE, TAKER_OVERRIDE, MANUAL_REVIEW_REQUIRED...
sqlite> SELECT ts,mode,starting_balance,ending_balance,net_pnl FROM runs;
```
Money columns are exact decimal strings (TEXT). Rebate totals: `weather-bot rebates`.
Daily CSVs: `/var/lib/weather-bot/exports/<table>_<YYYY-MM-DD>.csv`.

If the drawdown kill switch trips: `ls /var/lib/weather-bot/KILL_SWITCH`, review, then delete it to allow restarts.

## 4. Uninstall
```bash
sudo systemctl disable --now underhaven-weather
sudo rm -f /etc/systemd/system/underhaven-weather.service /usr/local/bin/weather-bot
sudo systemctl daemon-reload
sudo rm -rf /etc/weather-bot            # config + secrets
sudo rm -rf /var/lib/weather-bot        # database + exports (back up first)
```
