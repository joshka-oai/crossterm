# Mouse Scroll Example: Burst Timing + EventStream Plan

## Goals

- Track and surface scroll burst timing to compare terminal behavior.
- Render at most 60fps, only when needed (events or active burst).
- Process events as they arrive and keep rendering decoupled from timing calculations.
- Show rich per-event and per-burst timing details in the debug pane.
- Support auto vs manual burst timeout with live feedback.

## Current Behavior

- Event loop uses tokio + `EventStream` with a 60fps interval.
- Rendering runs only when needed, plus while a burst is active.
- Rendering uses the alternate screen and synchronized updates.
- Scrolls line-by-line with a selectable step (1 or 3).
- Burst boundaries happen on timeout or direction change.
- Per-event log lines show direction, delta (`Δt`), elapsed (`t`), and coordinates.
- Burst summaries are logged when a burst closes (count, duration, avg delta).
- Burst log entries alternate blue/cyan to group bursts visually.
- Auto timeout uses median + 3 * MAD from recent inter-event gaps.
- Manual timeout defaults to 120ms and adjusts in 10ms steps.
- Content source cycles between lipsum, design doc, and source code.

## Controls

- `q`/`Esc`: quit
- `1`/`3`: scroll step
- `a`: auto/manual timeout
- `[`/`]`: manual timeout -/+
- `t`: content source (lipsum/design/source)
- `d`: debug pane on/off
- `r`: reset counters and burst state
- arrows: scroll line-by-line

## Debug Pane Layout

- Help block at the top.
- Blank spacer line.
- Labeled stats block (Active, Last, Gap, Cal, Source, Timeout).
- Horizontal border.
- Explanation block (legend for `Δt`, `t`, Active/Last, Burst).
- Event log entries after the legend.

## Plan

### 1. Burst timing model + state

- Track burst lifecycle:
  - `burst_start: Option<Instant>`
  - `burst_last_event: Option<Instant>`
  - `burst_event_count: u32`
  - `burst_direction: Option<ScrollDirection>`
- Per-burst stats for UI:
  - last completed burst duration, count, average delta
  - rolling average events-per-burst and avg delta
- Timeout modes:
  - Manual timeout value (ms)
  - Auto mode derived from recent gaps using median + MAD (robust to spikes)
- Expose current mode and effective timeout in debug header.
- Burst boundary rule: direction change ends the current burst (chosen).
  - Not chosen: track independent bursts per direction concurrently.

### 2. Event loop refactor (tokio + EventStream)

- Replace blocking `event::read()` loop with `EventStream`.
- Add a 60fps `tokio::time::interval`.
- Maintain a `render_needed` flag set by:
  - incoming events (mouse, resize, key)
  - active burst aging (until it closes)
- On each tick:
  - if `render_needed`, render and clear flag
- On event:
  - handle event, mark `render_needed`
  - do not drain the queue; calculations use event timestamps and should remain
    independent of render cadence

### 3. Debug pane updates

- Per-event lines include:
  - `Δt` since previous event
  - `t` since burst start
  - direction / kind
- Live “Active burst” line showing:
  - duration so far, event count, avg delta so far
- On burst close:
  - append summary line: events, total duration, avg delta
- Show timeout mode (auto/manual) and effective threshold.

### 4. Controls / UX

- Keys:
  - Toggle auto/manual timeout
  - Adjust manual timeout up/down
  - Reset burst/calibration state
- Debug pane default on; toggle remains.
- Ensure layout rewraps when debug pane toggles.

## Open Questions

- Exact auto-timeout formula: median + MAD chosen for robustness.
  - Not chosen: mean + k * stdev (more sensitive to spikes).
- Whether to cap events per frame: not needed since rendering is decoupled.

## Debug Pane Alignment

The debug pane uses a fixed-width label column so values line up vertically even as
numeric values grow. Labels are left-aligned and data is padded so the rows read like
a table. Duration values always render with the same precision (`ms` with three
decimals) to avoid jitter when counts change.

## Median + MAD Timeout

Auto timeout uses the median inter-event gap as the typical spacing between scroll
events. The median is robust to occasional spikes that would skew a mean.

MAD (median absolute deviation) is the median of the absolute differences from that
median gap. It captures the typical variability without being thrown off by outliers.

The effective timeout is computed as `median + 3 * MAD`, which tolerates jitter while
still closing bursts once the gap meaningfully exceeds normal behavior.
