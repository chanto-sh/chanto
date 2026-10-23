// SPDX-License-Identifier: Apache-2.0
// Run with a TypeScript runner, e.g. npx tsx sdk/examples/ticket_agent.ts
import { query } from "../typescript/index";

async function main() {
  console.log(await query("Look up ticket 42 and suggest a plan to resolve it.", {
    mode: "bypassPermissions", allowedTools: ["lookup_ticket"],
    tools: [{ name: "lookup_ticket", description: "Look up a ticket by ID",
      inputSchema: { type: "object", properties: { id: { type: "integer" } }, required: ["id"] },
      handler: async ({ id }) => ({ id, title: "Fix login validation", status: "open" }),
    }],
  }));
}
main().catch(e => { console.error(e); process.exitCode = 1; });
