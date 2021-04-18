import sqlite3

if __name__ == '__main__':

    conn = sqlite3.connect('mount/users.db')
    c = conn.cursor()

    users = c.execute('SELECT user_id, enabled FROM users')

    insert_tuples = []
    for uid, enabled in users:
        if not enabled:
            insert_tuples.append(('enable', enabled, uid))

    c.executemany('INSERT INTO user_settings VALUES(?,?,?);', insert_tuples)
    conn.commit()
