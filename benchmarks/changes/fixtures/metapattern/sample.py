# print(x) mentioned in a comment, not a real call site.
def real_matches(x):
    print(x)
    log(x)
    pprint(x)
    s = "print(x) inside a string, not a call"
    return s
