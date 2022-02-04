from helpers.database_helper import UserDatabase, StatisticsDatabase


class BotManager:
    def __init__(self, ):

        self.users_db = UserDatabase()
        self.messages_db = StatisticsDatabase()
        pass

    async def get_user_data(self, user_id):
        await self.users_db.get_user_data(user_id)
        pass
