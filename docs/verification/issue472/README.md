Closes #472.

The key-hold cap already landed in `ebc9ad8`. This PR adds the missing real-terminal acceptance case: slow punctuation input through production KWin stdio, independently exported application bytes, and complete per-request key evidence.

On engi, production server `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` delivered all **34 requested bytes exactly** to a real Konsole shell reader. The payload contains a hyphenated pathname and an intentional double hyphen; it is read literally and never executed. At `speed: 0.1`, the RPC took 40.975 seconds. The input journal contains all 34 presses and releases, every event is marked flushed, and all 34 observed holds exceeded 200 ms. The longest observed hold was 261.610 ms, including scheduling delay.

Ruby syntax and the complete private desktop probe passed. The screenshot, exact request/output, input journal, and measured receipt are committed under `docs/verification/issue472/`. This finite run establishes the reported scenario; it does not prove behavior under every possible host stall. Runtime source is unchanged.
