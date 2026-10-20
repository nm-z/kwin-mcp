# Window activation through listed IDs

Closes #497.

The current runtime already normalizes KWin window UUIDs. This PR supplies acceptance coverage of the reported activation path: two real dialogs, eight forced focus switches using exactly listed, unbraced, braced, and uppercase identifiers, and independent verification of the resulting keyboard focus.

The built production stdio server on engi (`05b937d1c2ce50c52b80c878e0d1c12555a3ef44`) passed all eight activation checks. Each dialog then accepted its own text and exported the exact expected bytes: `first497\n` and `second497\n`. The regression uses private state, fresh profiles, and owned dialogs, and verifies cleanup.

Validation: Ruby syntax check and `KWIN_MCP_TEST_TMPDIR=/home/nate/km-ec25 ruby tests/window_activation_ids_e2e.rb /path/to/kwin-mcp /path/to/evidence` both passed.

The committed record in `docs/verification/issue497/` includes the actual activation IDs, independently accepted output, and lossless captures. Runtime source is unchanged.
