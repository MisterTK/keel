"""Prints sys.argv so tests can assert argv passthrough (and its byte-identity
between a plain `python app ...` run and `keel run app ...`)."""

import sys

print(sys.argv)
