from app.app import app
from app.config import HOST,PORT
from app.bot import bot
bot.start()
app.run(host=HOST,port=PORT)
