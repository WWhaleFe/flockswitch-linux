# Mac Caps Lock long press: capture contract

Spec: APP_SPEC_2_1.md A4, `DirRules.caps_long_press` (mac source only, default true).
Caps Lock held 300 ms or more is sent as a real Caps Lock; a shorter press is one tap of
the receiver's Korean/English key (`hangul_dst`).

## What netcore expects

`keymap::KeyMapper` decides from the real length of the press:

1. `Key { usage: 0x39, down: true }` starts a pending press and sends nothing yet.
2. While the key stays down, the service loop calls `KeyMapper::tick` (every 8 ms, `POLL`).
   Once 300 ms (`CAPS_LONG_PRESS`) have passed it sends Caps Lock down to the receiver
   and the press is "real".
3. `Key { usage: 0x39, down: false }`:
   - real press: Caps Lock up is sent, nothing else;
   - pending press shorter than 300 ms: one tap of `hangul_dst` (held Shift/Ctrl lifted
     around it and restored);
   - pending press of 300 ms or more that `tick` did not see yet (loop was busy): Caps
     Lock down and up.

So netcore needs the down event when the key goes down and the up event when it comes
back up, with the real time between them. The time comes from one of two sources:

- Preferred: deliver the two events when they happen, with a gap. The service stamps each
  event with `Instant::now()` when it takes it from the capture channel, which is the
  same moment as long as the capture thread sends immediately and the service loop is not
  stalled.
- Fallback for a capture that cannot keep the gap (or a queue that delays events): the
  `Platform` trait has two optional hooks, both default `None`:
  - `captured_at(&InputEvent) -> Option<Instant>`: the monotonic time the platform
    captured that event, asked once per event in channel order;
  - `caps_hold_ms() -> Option<u64>`: asked only on a Caps Lock up, the measured length of
    that press in milliseconds. It is used instead of the time between the two events and
    describes that one press only.

If neither is provided, a synthetic down+up pair (as the Mac capture sends today) reads
as a press of about 0 ms: always a short tap, never a real Caps Lock. That is the legacy
behaviour and still works for the Korean/English switch; only the long press is lost.

## What the mac capture code must change

Today `src/net.rs` (the `flagsChanged` branch, event kind 12) reacts to a flip of bit 16
(`1 << 16`) in the event flags with `caps_press(...)` and, for each new press, sends
`Key { usage: 0x39, down: true }` and `Key { usage: 0x39, down: false }` back to back.
Both leave within microseconds, so netcore sees a zero length press.

Required change, either of:

A. Real timing (preferred). On the flag flip that begins a press send only
   `Key { usage: 0x39, down: true }` and remember the time (`Instant::now()`). Send
   `Key { usage: 0x39, down: false }` when the press ends. The end of a press is not a
   second flip when macOS's "use Caps Lock to switch input source" is on (a short press
   flips on and back off at once on release; `caps_press` already recognises that off as
   belonging to the same press within `CAPS_SAME_PRESS`, 400 ms). Rules for the end:
   - plain Caps Lock: the flip to off at the next press is not the release of the first;
     the release has to come from the key itself. Use a key-up of keycode 0x39 if the
     event tap delivers one (on the current capture only flagsChanged, kind 12, arrives for
     Caps Lock; check it on hardware), or
     treat a flip to on followed by a flip to off within `CAPS_SAME_PRESS` as down/up with
     those two times, and a lone flip to on as down with the up sent on the next flip or
     when the physical key is seen up (poll `CGEventSourceKeyState(.., 0x39)` every
     10-20 ms while pending);
   - never send the up before the key is physically up, or the length is lost again.
   - Send the two events from the capture thread at the moments they happen (do not
     queue them up and send both at the end).

B. Keep the pair, add the hint. Keep sending down+up on the flip, but measure the press
   (time from the flip to on until the release as above) in the capture thread, store it,
   and implement `Platform::caps_hold_ms` for the Mac platform to return it once, right
   after the up event has been consumed (clear it on read so it describes one press only).
   For this the up must still be sent after the physical release, not at the flip.

In both cases the key remains swallowed below the event tap as it is now, the
Korean/English tap and the 300 ms boundary are decided only in netcore, and nothing else
in the capture changes. Tests: netcore has unit tests for the boundary
(`collapsed_caps_events_use_hold_hint_at_300ms_boundary`) and for down/up with a gap; the
mac side needs a test of the capture helper for a press of 120 ms and one of 700 ms.

Other ports: Windows, Linux and Android are not mac sources, so `caps_long_press` does
nothing there.
