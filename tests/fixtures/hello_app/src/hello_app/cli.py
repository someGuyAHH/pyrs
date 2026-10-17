import argparse
import os
import sys
import time

import greeter_dep


def main():
    parser = argparse.ArgumentParser(prog="hello-app")
    parser.add_argument("name", nargs="?", default="world")
    parser.add_argument("--exit", type=int, default=0, dest="code")
    parser.add_argument("--sleep", type=float, default=0)
    args = parser.parse_args()

    print(greeter_dep.greet(args.name))
    print("argv0=" + sys.argv[0])
    print("prefix=" + sys.prefix)
    print("dep=" + greeter_dep.__file__)
    print("cwd=" + os.getcwd())
    sys.stdout.flush()
    if args.sleep:
        time.sleep(args.sleep)
    return args.code
