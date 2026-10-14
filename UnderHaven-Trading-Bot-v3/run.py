"""Entry point for the systemd service: starts the persistent scanner (independent of any browser) and then the web server on localhost."""
from app.app import app
from app.config import HOST, PORT
from app import runtime

if __name__ == '__main__':
    started = runtime.start()      # False when another scanner process already holds the lock (no duplicate workers)
    app.run(host=HOST, port=PORT, threaded=True)
