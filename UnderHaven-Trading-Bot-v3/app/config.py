import os
from pathlib import Path
DATA_DIR=Path(os.getenv('UNDERHAVEN_DATA_DIR','/var/lib/underhaven-trading'))
DB_PATH=DATA_DIR/'underhaven.db'
VAULT_KEY_FILE=Path(os.getenv('UNDERHAVEN_VAULT_KEY_FILE','/etc/underhaven-trading/vault.key'))
CLOB_URL=os.getenv('POLYMARKET_CLOB_URL','https://clob.polymarket.com')
GAMMA_URL=os.getenv('POLYMARKET_GAMMA_URL','https://gamma-api.polymarket.com')
CHAIN_ID=int(os.getenv('POLYMARKET_CHAIN_ID','137'))
HOST=os.getenv('UNDERHAVEN_HOST','127.0.0.1')
PORT=int(os.getenv('UNDERHAVEN_PORT','5000'))
