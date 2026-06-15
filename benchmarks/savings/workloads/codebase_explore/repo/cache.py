"""A tiny cache used by the background service."""


class Cache:
    def __init__(self, ttl):
        self.ttl = ttl
        self.store = {}

    def get(self, key):
        return self.store.get(key)

    def set(self, key, value):
        self.store[key] = value


def build_cache(ttl):
    return Cache(ttl)
