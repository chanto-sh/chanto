#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run: PYTHONPATH=sdk/python python3 sdk/examples/fix_tests.py /path/to/repo"""
import sys
from chanto_sdk import query

print(query('Run the tests, fix any failures, and report the changes and verification results.',
    cwd=sys.argv[1] if len(sys.argv) > 1 else '.', verify=True,
    mode='bypassPermissions', max_turns=50))
