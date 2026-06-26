"""Calibration corpus for the Python tags adapter.

Exercises every shape the Python tags.scm and the hand-written Python spec both
see: module-level constants (tags-only: @definition.constant), classes, functions,
methods, and calls (bare identifier and attribute). The committed delta between the
two extractions is frozen by bench_calibration.
"""

import os
from collections import OrderedDict

MAX_RETRIES = 3
DEFAULT_NAME = "anon"


def helper(value):
    return value * 2


def compute(items):
    total = 0
    for item in items:
        total = total + helper(item)
    return total


class Widget:
    def __init__(self, name):
        self.name = name or DEFAULT_NAME

    def render(self):
        return greet(self.name)

    def total(self, items):
        return compute(items)


class Panel(Widget):
    def render(self):
        base = Widget.render(self)
        return base.upper()


def greet(name):
    return "hi " + name


def main():
    panel = Panel("demo")
    print(panel.render())
    print(compute([1, 2, MAX_RETRIES]))
    os.getpid()
