_DB = {}


def _encode(value):
    return value.encode("utf-8")


def write(value):
    _DB[value] = _encode(value)


def read(key):
    return _DB.get(key)


class Cache:
    def __init__(self):
        self.data = {}

    def write(self, key, value):
        self.data[key] = value

    def read(self, key):
        return self.data.get(key)
