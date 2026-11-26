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

If neither hook is provided, a synthetic down/up pair reads as a press of about
0 ms. It can switch Korean/English but cannot preserve a long hold.

## Current mac capture

`src/caps_macos.m` observes raw keyboard usage 0x39 through IOHIDManager. It sends
physical down and up edges independently of alpha-shift `flagsChanged`. The latter
can arrive only on release when Caps Lock switches input sources, and cannot measure
a hold or distinguish two rapid presses.

The raw observer combines held state across keyboards and releases a held key when
its device is removed. It opens devices without seizing them. The event tap continues
to swallow local keyboard events, including Caps Lock state flips.

`src/net.rs` forwards those physical edges and implements `Platform::captured_at`.
IOHIDValue timestamps are converted to monotonic `Instant`s and consumed in channel
order, so a busy capture or service loop does not collapse a long hold into a tap.
The timestamp queue is cleared when a capture session starts.

Tests cover rapid physical presses, repeated held state, a batched 700 ms press,
and netcore's short/long press decisions at the 300 ms boundary. Hardware input and
input-source switching still need a macOS session with Input Monitoring permission.

Other ports: Windows, Linux and Android are not mac sources, so `caps_long_press`
does nothing there.
