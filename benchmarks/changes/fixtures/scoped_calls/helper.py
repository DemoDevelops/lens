# Python `shared_name`: same name as helper.rs's Rust fn. py_drive's call must
# resolve same-file (Python -> Python); no Rust caller may ever link here.
def shared_name(n):
    return n


def py_drive(n):
    return shared_name(n)
