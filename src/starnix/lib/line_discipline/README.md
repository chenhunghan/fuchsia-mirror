# Line Discipline

This crate implements the terminal line discipline logic for Starnix.

## Purpose

The line discipline is responsible for the intermediate processing of characters between the terminal device (e.g., a PTY master or a real serial port) and the reading process (e.g., bash). Its responsibilities include:

*   **Canonical Mode Processing**: Buffering input line-by-line, handling backspace (`\x08` or `\x7f`), word erase (`^W`), line kill (`^U`), literal-next (`^V`), reprint (`^R`), and EOF/EOL delimiters (`^D`, `VEOL`, `VEOL2`).
*   **Echoing**: Echoing typed characters back to the output, potentially transforming them (e.g., `ECHOCTL`, `ECHOE`, `ECHOK`, `ECHOKE`, `ECHONL`, `ECHOPRT`).
*   **Signal Generation**: Detecting special control characters (like `^C`, `^\`, `^Z`) and generating corresponding signals (`SIGINT`, `SIGQUIT`, `SIGTSTP`).
*   **Input & Output Processing**: Transforming input (`ISTRIP`, `PARMRK`, `INLCR`, `IGNCR`, `ICRNL`, `IUCLC`, `IXON`, `IXANY`, `IUTF8`) and output (`OPOST`, `OLCUC`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`, `TABDLY`/`XTABS`) characters, as well as `EXTPROC` and `TIOCPKT` packet mode.

## Notes on Flow Control

While the `LineDiscipline` struct maintains `IXON` and `IXOFF` flags, automatic input flow control (throttling) via `IXOFF` (sending `VSTOP`/`VSTART` when the input buffer fills/empties) is **not implemented**. This matches the behavior of Linux PTYs, which only apply throttling logic to hardware serial devices, not pseudo-terminals. PTYs rely on producer-consumer backpressure rather than in-band flow control characters.

## Architecture

This logic was extracted from `starnix_core` to decouple it from the kernel structures and allow for easier testing and potential reusability.

Key components:
*   `LineDiscipline`: The main state struct holding the termios configuration, queues, and cursor state.
*   `Queue`: Manages the flow of data, handling wait buffers (raw data) and read buffers (processed data).
*   `InputBuffer` / `OutputBuffer` traits: Abstractions for the data sources/sinks to avoid direct dependencies on Starnix kernel buffer types.

## Testing

Unit tests are defined in `lib.rs` (`line_discipline_tests`), and Linux cross-tested trace scenarios are defined in `testing/` (`line_discipline_scenarios_tests`).
