# Host/session boundary verification

PR #517 covers issues #490, #511, and #515. This revision preserves the concurrent
SSH trust and source-file hardening in 30dda52 and closes a launch preflight race.

A no-window launch now writes its PID marker only after path validation. The
reader stops promptly on a path error or early exit. Non-window launches also skip
window-baseline discovery. Existing submission and retry-safety metadata remain intact.

## Production stdio checks on engi

All six tests passed using the built server in fresh, isolated KWin sessions:

| Test | Result |
| --- | --- |
| launch_app_without_expected_window_returns_its_pid_promptly | 1 passed; 3.18 s |
| session_shares_only_host_shared_dir_with_the_host | 1 passed; 4.27 s |
| session_ssh_drop_ins_are_owned_by_the_session_user | 1 passed; 4.12 s |
| launch_app_names_a_socket_path_in_a_host_directory_the_session_cannot_see | 1 passed; 2.55 s |
| launch_app_without_expected_window_rejects_invisible_socket_path | 1 passed; 3.16 s |
| launch_app_reports_commands_that_exit_without_a_window | 1 passed; 3.56 s |

The shared-directory case now opens a real Unix socket in the session, connects
from the host, and reads the expected reply. It also verifies private /tmp,
host-visible file output, and stdout/stderr behavior.

The SSH case checks actual session ownership/modes and runs ssh -G against a
nonexistent example host; it does not make a network connection.

Run each exact test with KWIN_MCP_E2E=1, KWIN_MCP_E2E_SERVER pointing to the built
server, and --ignored --exact --nocapture --test-threads=1. The fixture creates and
stops its own desktop. Focused Rust formatting and git diff --check pass.
