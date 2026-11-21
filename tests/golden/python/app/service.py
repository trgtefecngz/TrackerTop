from app import storage
from app.models import Admin, User


def register(name):
    user = Admin(name) if name.startswith("admin") else User(name)
    user.save()
    return user


def lookup(key):
    return storage.read(key)


def cached_lookup(cache, key):
    found = cache.read(key)
    if found is None:
        found = lookup(key)
        cache.write(key, found)
    return found
