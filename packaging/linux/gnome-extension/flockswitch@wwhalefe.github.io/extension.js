// flockswitch helper: the pointer, the screen edges and the keyboard and mouse for the
// flockswitch app on GNOME Wayland, where an ordinary program can do none of that and
// the portals (InputCapture before GNOME 51, RemoteDesktop without Remember) ask the
// user again and again.
//
// D-Bus: io.github.wwhalefe.Flockswitch on the session bus, object
// /io/github/wwhalefe/Flockswitch, interface io.github.wwhalefe.Flockswitch1.
//
// Only the flockswitch program may call it: each caller's process must run an
// executable named "flockswitch". Captured input goes to that caller alone (unicast
// signals), never to the whole bus.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.wwhalefe.Flockswitch';
const OBJECT_PATH = '/io/github/wwhalefe/Flockswitch';
const IFACE = 'io.github.wwhalefe.Flockswitch1';
const API_VERSION = 1;
// A capture with no KeepAlive for this long is given back (the app hung or crashed).
const KEEPALIVE_MS = 3000;

const XML = `
<node>
  <interface name="${IFACE}">
    <method name="Hello"><arg type="u" direction="out" name="version"/></method>
    <method name="GetPointer">
      <arg type="d" direction="out" name="x"/><arg type="d" direction="out" name="y"/>
      <arg type="u" direction="out" name="mods"/>
    </method>
    <method name="GetMonitors"><arg type="a(iiii)" direction="out" name="monitors"/></method>
    <method name="Warp"><arg type="d" direction="in" name="x"/><arg type="d" direction="in" name="y"/></method>
    <method name="StartCapture"><arg type="b" direction="out" name="ok"/></method>
    <method name="StopCapture"/>
    <method name="KeepAlive"/>
    <method name="Motion"><arg type="d" direction="in" name="dx"/><arg type="d" direction="in" name="dy"/></method>
    <method name="MoveTo"><arg type="d" direction="in" name="x"/><arg type="d" direction="in" name="y"/></method>
    <method name="Button"><arg type="u" direction="in" name="code"/><arg type="b" direction="in" name="down"/></method>
    <method name="Key"><arg type="u" direction="in" name="code"/><arg type="b" direction="in" name="down"/></method>
    <method name="Scroll"><arg type="d" direction="in" name="dx"/><arg type="d" direction="in" name="dy"/></method>
    <signal name="CapturedMotion"><arg type="d" name="dx"/><arg type="d" name="dy"/></signal>
    <signal name="CapturedButton"><arg type="u" name="code"/><arg type="b" name="down"/></signal>
    <signal name="CapturedKey"><arg type="u" name="code"/><arg type="b" name="down"/></signal>
    <signal name="CapturedScroll"><arg type="d" name="dx"/><arg type="d" name="dy"/></signal>
    <signal name="CaptureEnded"/>
  </interface>
</node>`;

// Clutter button numbers to Linux evdev codes (BTN_LEFT ...).
const BUTTON_TO_EVDEV = {1: 0x110, 2: 0x112, 3: 0x111, 8: 0x113, 9: 0x114};
const EVDEV_TO_BUTTON = {0x110: 1, 0x111: 3, 0x112: 2, 0x113: 8, 0x114: 9};

function seat() {
    const backend = global.stage.context?.get_backend?.() ?? Clutter.get_default_backend();
    return backend.get_default_seat();
}

function cursorTracker() {
    return global.backend?.get_cursor_tracker?.() ?? Meta.CursorTracker.get_for_display(global.display);
}

function now() {
    return GLib.get_monotonic_time();
}

export default class FlockswitchHelper extends Extension {
    enable() {
        this._allowed = new Map(); // unique bus name -> true/false
        this._capture = null;
        this._pointer = null;
        this._keyboard = null;
        this._held = {keys: new Set(), buttons: new Set()};
        this._impl = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._impl.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS_NAME,
            Gio.BusNameOwnerFlags.REPLACE, null, null);
    }

    disable() {
        this._endCapture(false);
        this._releaseHeld();
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        this._impl?.unexport();
        this._impl = null;
        this._pointer = null;
        this._keyboard = null;
        this._allowed = null;
    }

    // --- who may call ---------------------------------------------------------------

    _isFlockswitch(sender) {
        if (this._allowed.has(sender))
            return this._allowed.get(sender);
        let ok = false;
        try {
            const reply = Gio.DBus.session.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetConnectionUnixProcessID', new GLib.Variant('(s)', [sender]),
                new GLib.VariantType('(u)'), Gio.DBusCallFlags.NONE, 1000, null);
            const [pid] = reply.deepUnpack();
            const exe = GLib.file_read_link(`/proc/${pid}/exe`);
            // Installed copies only: install.sh / self-update (~/.local/bin) or Homebrew.
            const home = GLib.get_home_dir();
            ok = GLib.path_get_basename(exe) === 'flockswitch' &&
                (exe === `${home}/.local/bin/flockswitch` ||
                 /\/Cellar\/flockswitch\/[^/]+\/bin\/flockswitch$/.test(exe));
        } catch (e) {
            ok = false;
        }
        this._allowed.set(sender, ok);
        return ok;
    }

    _guard(invocation) {
        const sender = invocation.get_sender();
        if (this._isFlockswitch(sender))
            return sender;
        invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied',
            'Only the flockswitch program may use this.');
        return null;
    }

    // --- reading ----------------------------------------------------------------------

    HelloAsync(_params, invocation) {
        if (!this._guard(invocation))
            return;
        invocation.return_value(new GLib.Variant('(u)', [API_VERSION]));
    }

    GetPointerAsync(_params, invocation) {
        if (!this._guard(invocation))
            return;
        const [x, y, mods] = global.get_pointer();
        invocation.return_value(new GLib.Variant('(ddu)', [x, y, mods]));
    }

    GetMonitorsAsync(_params, invocation) {
        if (!this._guard(invocation))
            return;
        const list = Main.layoutManager.monitors.map(m => [m.x, m.y, m.width, m.height]);
        invocation.return_value(new GLib.Variant('(a(iiii))', [list]));
    }

    WarpAsync([x, y], invocation) {
        if (!this._guard(invocation))
            return;
        seat().warp_pointer(Math.round(x), Math.round(y));
        invocation.return_value(null);
    }

    // --- injecting (input from the paired computer that controls this one) --------------

    _pointerDevice() {
        this._pointer ??= seat().create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
        return this._pointer;
    }

    _keyboardDevice() {
        this._keyboard ??= seat().create_virtual_device(Clutter.InputDeviceType.KEYBOARD_DEVICE);
        return this._keyboard;
    }

    MotionAsync([dx, dy], invocation) {
        if (!this._guard(invocation))
            return;
        this._pointerDevice().notify_relative_motion(now(), dx, dy);
        invocation.return_value(null);
    }

    MoveToAsync([x, y], invocation) {
        if (!this._guard(invocation))
            return;
        this._pointerDevice().notify_absolute_motion(now(), x, y);
        invocation.return_value(null);
    }

    ButtonAsync([code, down], invocation) {
        if (!this._guard(invocation))
            return;
        if (code in EVDEV_TO_BUTTON) {
            this._pointerDevice().notify_button(now(), code,
                down ? Clutter.ButtonState.PRESSED : Clutter.ButtonState.RELEASED);
            if (down)
                this._held.buttons.add(code);
            else
                this._held.buttons.delete(code);
        }
        invocation.return_value(null);
    }

    KeyAsync([code, down], invocation) {
        if (!this._guard(invocation))
            return;
        this._keyboardDevice().notify_key(now(), code,
            down ? Clutter.KeyState.PRESSED : Clutter.KeyState.RELEASED);
        if (down)
            this._held.keys.add(code);
        else
            this._held.keys.delete(code);
        invocation.return_value(null);
    }

    ScrollAsync([dx, dy], invocation) {
        if (!this._guard(invocation))
            return;
        // dx, dy in wheel clicks; Clutter's smooth scroll uses 10 units a click.
        this._pointerDevice().notify_scroll_continuous(now(), dx * 10, dy * 10,
            Clutter.ScrollSource.WHEEL, Clutter.ScrollFinishFlags.NONE);
        invocation.return_value(null);
    }

    _releaseHeld() {
        if (!this._held)
            return;
        for (const code of this._held.keys)
            this._keyboard?.notify_key(now(), code, Clutter.KeyState.RELEASED);
        for (const code of this._held.buttons)
            this._pointer?.notify_button(now(), code, Clutter.ButtonState.RELEASED);
        this._held.keys.clear();
        this._held.buttons.clear();
    }

    // --- capturing (this computer's mouse and keyboard go to another computer) ----------

    StartCaptureAsync(_params, invocation) {
        const sender = this._guard(invocation);
        if (!sender)
            return;
        if (this._capture && this._capture.sender !== sender)
            this._endCapture(true);
        if (!this._capture) {
            const actor = new Clutter.Actor({reactive: true, width: 1, height: 1, opacity: 0});
            Main.layoutManager.uiGroup.add_child(actor);
            const grab = Main.pushModal(actor, {actionMode: Shell.ActionMode.NONE});
            if (!(grab.get_seat_state() & Clutter.GrabState.ALL)) {
                Main.popModal(grab);
                actor.destroy();
                invocation.return_value(new GLib.Variant('(b)', [false]));
                return;
            }
            const [ax, ay] = global.get_pointer();
            const capture = {sender, actor, grab, anchor: [ax, ay], lastAlive: now(), eventId: 0,
                watchId: 0, timerId: 0};
            // Recorded at once, so a failure below still gives the grab back.
            this._capture = capture;
            try {
                capture.eventId = global.stage.connect('captured-event', (_a, event) => this._onEventSafe(event));
                capture.watchId = Gio.bus_watch_name_on_connection(Gio.DBus.session, sender,
                    Gio.BusNameWatcherFlags.NONE, null, () => this._endCapture(true));
                capture.timerId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 500, () => {
                    if (now() - capture.lastAlive > KEEPALIVE_MS * 1000) {
                        capture.timerId = 0;
                        this._endCapture(true);
                        return GLib.SOURCE_REMOVE;
                    }
                    return GLib.SOURCE_CONTINUE;
                });
                cursorTracker().set_pointer_visible(false);
            } catch (e) {
                logError(e, 'flockswitch helper: starting the capture');
                this._endCapture(false);
                invocation.return_value(new GLib.Variant('(b)', [false]));
                return;
            }
        }
        invocation.return_value(new GLib.Variant('(b)', [true]));
    }

    StopCaptureAsync(_params, invocation) {
        if (!this._guard(invocation))
            return;
        this._endCapture(false);
        invocation.return_value(null);
    }

    KeepAliveAsync(_params, invocation) {
        if (!this._guard(invocation))
            return;
        if (this._capture)
            this._capture.lastAlive = now();
        invocation.return_value(null);
    }

    _endCapture(tell) {
        const c = this._capture;
        if (!c)
            return;
        this._capture = null;
        // Each step on its own: one failing must not keep the others (above all the grab).
        const steps = [
            () => c.eventId && global.stage.disconnect(c.eventId),
            () => c.watchId && Gio.bus_unwatch_name(c.watchId),
            () => c.timerId && GLib.source_remove(c.timerId),
            () => Main.popModal(c.grab),
            () => c.actor.destroy(),
            () => cursorTracker().set_pointer_visible(true),
        ];
        for (const step of steps) {
            try {
                step();
            } catch (e) {
                logError(e, 'flockswitch helper: ending the capture');
            }
        }
        if (tell)
            this._send(c.sender, 'CaptureEnded', null);
    }

    _send(dest, name, params) {
        try {
            Gio.DBus.session.emit_signal(dest, OBJECT_PATH, IFACE, name, params);
        } catch (e) {
            // The app went away; the name watch ends the capture.
        }
    }

    // Never lets an exception hand captured input to the local desktop.
    _onEventSafe(event) {
        try {
            return this._onEvent(event);
        } catch (e) {
            logError(e, 'flockswitch helper: captured event');
            return this._capture ? Clutter.EVENT_STOP : Clutter.EVENT_PROPAGATE;
        }
    }

    _onEvent(event) {
        const c = this._capture;
        if (!c)
            return Clutter.EVENT_PROPAGATE;
        const type = event.type();
        // Emergency exit on this computer: Ctrl+Alt+Shift+Escape gives input back.
        if (type === Clutter.EventType.KEY_PRESS && event.get_key_symbol() === Clutter.KEY_Escape) {
            const need = Clutter.ModifierType.CONTROL_MASK | Clutter.ModifierType.MOD1_MASK |
                Clutter.ModifierType.SHIFT_MASK;
            if ((event.get_state() & need) === need) {
                this._endCapture(true);
                return Clutter.EVENT_STOP;
            }
        }
        switch (type) {
        case Clutter.EventType.MOTION: {
            // gboolean (dx, dy, ...) on GNOME 45+, older builds give (dx, dy) only.
            const rel = event.get_relative_motion?.() ?? [];
            const [dx, dy] = typeof rel[0] === 'boolean' ? (rel[0] ? [rel[1], rel[2]] : [0, 0]) : [rel[0], rel[1]];
            if (Number.isFinite(dx) && Number.isFinite(dy) && (dx !== 0 || dy !== 0))
                this._send(c.sender, 'CapturedMotion', new GLib.Variant('(dd)', [dx, dy]));
            // Keep the hidden pointer where the capture began; warps carry no relative motion.
            const [x, y] = global.get_pointer();
            if (x !== c.anchor[0] || y !== c.anchor[1])
                seat().warp_pointer(c.anchor[0], c.anchor[1]);
            return Clutter.EVENT_STOP;
        }
        case Clutter.EventType.BUTTON_PRESS:
        case Clutter.EventType.BUTTON_RELEASE: {
            const code = BUTTON_TO_EVDEV[event.get_button()];
            if (code !== undefined)
                this._send(c.sender, 'CapturedButton',
                    new GLib.Variant('(ub)', [code, type === Clutter.EventType.BUTTON_PRESS]));
            return Clutter.EVENT_STOP;
        }
        case Clutter.EventType.KEY_PRESS:
        case Clutter.EventType.KEY_RELEASE: {
            const code = event.get_key_code() - 8;
            if (code > 0)
                this._send(c.sender, 'CapturedKey',
                    new GLib.Variant('(ub)', [code, type === Clutter.EventType.KEY_PRESS]));
            return Clutter.EVENT_STOP;
        }
        case Clutter.EventType.SCROLL: {
            if (event.get_flags() & Clutter.EventFlags.POINTER_EMULATED)
                return Clutter.EVENT_STOP;
            let dx = 0, dy = 0;
            switch (event.get_scroll_direction()) {
            case Clutter.ScrollDirection.UP: dy = -1; break;
            case Clutter.ScrollDirection.DOWN: dy = 1; break;
            case Clutter.ScrollDirection.LEFT: dx = -1; break;
            case Clutter.ScrollDirection.RIGHT: dx = 1; break;
            case Clutter.ScrollDirection.SMOOTH: [dx, dy] = event.get_scroll_delta(); break;
            }
            if (dx !== 0 || dy !== 0)
                this._send(c.sender, 'CapturedScroll', new GLib.Variant('(dd)', [dx, dy]));
            return Clutter.EVENT_STOP;
        }
        default:
            // Touch, gestures, enter/leave: kept from the local desktop while captured.
            return Clutter.EVENT_STOP;
        }
    }
}
