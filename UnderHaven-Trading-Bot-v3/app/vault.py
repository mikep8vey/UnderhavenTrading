from cryptography.fernet import Fernet
from pathlib import Path
from .config import VAULT_KEY_FILE

class Vault:
    def __init__(self):
        VAULT_KEY_FILE.parent.mkdir(parents=True, exist_ok=True)
        if not VAULT_KEY_FILE.exists():
            VAULT_KEY_FILE.write_bytes(Fernet.generate_key())
            VAULT_KEY_FILE.chmod(0o600)
        self.fernet = Fernet(VAULT_KEY_FILE.read_bytes().strip())

    def encrypt(self, value: str) -> str:
        return self.fernet.encrypt(value.encode()).decode()

    def decrypt(self, value: str) -> str:
        return self.fernet.decrypt(value.encode()).decode()
