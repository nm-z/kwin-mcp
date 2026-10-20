Closes #476.

The existing production runtime supports an explicit typing speed. This PR documents how to use it in a focused terminal and adds a real Konsole acceptance case for a 120-character command, including Return, at `speed: 4`.

On engi, the production stdio server at `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` completed that RPC in **3.917 seconds**. All 120 characters were submitted and acknowledged, and an independent export matched the command's exact 76-byte output. The terminal receipt correctly remained `unverified` and did not suggest a blind retry.

Ruby syntax and the complete private desktop probe passed. The actual terminal screenshot, request, output, receipt, and timing record are committed under `docs/verification/issue476/`. This changes guidance and acceptance coverage; runtime timing defaults are unchanged.
