# Mouse Scroll Example: Burst Timing + EventStream Plan

## Goals

- Track and surface scroll burst timing to compare terminal behavior.
- Render at most 60fps, only when needed (events or active burst).
- Drain events aggressively without exceeding the frame budget.
- Show rich per-event and per-burst timing details in the debug pane.
- Support auto vs manual burst timeout with live feedback.

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
