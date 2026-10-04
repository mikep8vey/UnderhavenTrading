from app.app import app
from app.config import HOST, PORT

# The Bot instance starts its worker thread when imported. Keeping the process
# itself focused on Flask means systemd owns the long-running service while the
# scanner thread continues independently of browser sessions.

if __name__ == '__main__':
    app.run(host=HOST, port=PORT)
