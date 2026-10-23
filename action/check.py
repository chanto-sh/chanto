#!/usr/bin/env python3
"""Verified-mode gate: run one trusted check inside the Chanto sandbox."""
import json
import os
import sys

from review import run_checks

if __name__ == "__main__":
    sys.exit(0 if run_checks(dict(os.environ), {"checks": [sys.argv[1]]}) else 1)
