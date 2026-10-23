#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run: PYTHONPATH=sdk/python python3 sdk/examples/ticket_agent.py"""
from chanto_sdk import query, tool

@tool('lookup_ticket', 'Look up a ticket by ID', {
    'type': 'object', 'properties': {'id': {'type': 'integer'}}, 'required': ['id']})
def lookup_ticket(args):
    return {'id': args['id'], 'title': 'Fix login validation', 'status': 'open'}

print(query('Look up ticket 42 and suggest a plan to resolve it.', tools=[lookup_ticket],
    allowed_tools=['lookup_ticket'], mode='bypassPermissions'))
