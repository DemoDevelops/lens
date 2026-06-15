"""Background processing service."""

from cache import build_cache


def process(items):
    cache = build_cache(300)
    results = []
    for item in items:
        cached = cache.get(item)
        if cached is None:
            cached = transform(item)
            cache.set(item, cached)
        results.append(cached)
    return results


def transform(item):
    return item.strip().upper()
