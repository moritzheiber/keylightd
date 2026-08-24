import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import {Extension, gettext as _} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Slider} from 'resource:///org/gnome/shell/ui/slider.js';

const BUS_NAME = 'im.heiber.keylightd';
const ROOT_PATH = '/im/heiber/keylightd';

const BRIGHTNESS_MIN = 1;
const BRIGHTNESS_MAX = 100;
const KELVIN_MIN = 2900;
const KELVIN_MAX = 7000;
const BRIGHTNESS_STEP = 5;
// While a slider is dragged it emits notify::value on every motion step. Each
// send becomes a D-Bus call and an HTTP request to the light, so coalesce them:
// send the leading edge immediately, then at most one update per interval, and
// always flush the settled value when the drag ends.
const SLIDER_THROTTLE_MS = 150;
const KEYBINDINGS = ['brightness-up', 'brightness-down', 'toggle-power'];

// Directory holding the extension's bundled symbolic icons; set in enable().
let iconsDir = null;

function extensionIcon(name) {
    return Gio.icon_new_for_string(`${iconsDir}/${name}.svg`);
}

const ROOT_XML = `
<node>
  <interface name="im.heiber.keylightd1">
    <property name="CameraActive" type="b" access="read"/>
    <property name="HasPreset" type="b" access="read"/>
    <property name="LightPaths" type="ao" access="read"/>
    <method name="SavePreset"/>
    <method name="ApplyPreset"/>
  </interface>
</node>`;

const LIGHT_XML = `
<node>
  <interface name="im.heiber.keylightd1.Light">
    <property name="Id" type="s" access="read"/>
    <property name="Name" type="s" access="read"/>
    <property name="On" type="b" access="read"/>
    <property name="Brightness" type="y" access="read"/>
    <property name="TemperatureKelvin" type="q" access="read"/>
    <property name="Reachable" type="b" access="read"/>
    <method name="SetPower"><arg name="on" type="b" direction="in"/></method>
    <method name="TogglePower"/>
    <method name="SetBrightness"><arg name="brightness" type="y" direction="in"/></method>
    <method name="AdjustBrightness"><arg name="delta" type="i" direction="in"/></method>
    <method name="SetTemperatureKelvin"><arg name="kelvin" type="q" direction="in"/></method>
    <method name="AdjustTemperatureKelvin"><arg name="delta" type="i" direction="in"/></method>
  </interface>
</node>`;

const RootProxy = Gio.DBusProxy.makeProxyWrapper(ROOT_XML);
const LightProxy = Gio.DBusProxy.makeProxyWrapper(LIGHT_XML);

function clamp(value, low, high) {
    return Math.min(high, Math.max(low, value));
}

function lerp(from, to, ratio) {
    return Math.round(from + (to - from) * ratio);
}

// Convert a 0..1 slider position to the applied device value, matching exactly
// what the throttled setter sends so the on-screen readout never disagrees.
function brightnessFromSlider(value) {
    return clamp(Math.round(value * BRIGHTNESS_MAX), BRIGHTNESS_MIN, BRIGHTNESS_MAX);
}

function kelvinFromSlider(value) {
    return clamp(
        Math.round(KELVIN_MIN + value * (KELVIN_MAX - KELVIN_MIN)),
        KELVIN_MIN,
        KELVIN_MAX);
}

// Warm (2900 K) to cool (7000 K) tint for the temperature icon.
function kelvinColor(kelvin) {
    const ratio = clamp((kelvin - KELVIN_MIN) / (KELVIN_MAX - KELVIN_MIN), 0, 1);
    const warm = [255, 149, 67];
    const mid = [255, 224, 189];
    const cool = [176, 205, 255];
    const blend = (a, b, t) => [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)];
    const color = ratio < 0.5 ? blend(warm, mid, ratio / 0.5) : blend(mid, cool, (ratio - 0.5) / 0.5);
    return `rgb(${color[0]}, ${color[1]}, ${color[2]})`;
}

// A half-width preset tile holding an icon beside a label, expanding to share the
// row equally with its sibling. Modelled on the system Quick Settings tiles.
function makePresetTile(iconName, label, onClick) {
    const box = new St.BoxLayout({style_class: 'keylightd-preset-tile-content'});
    box.add_child(new St.Icon({icon_name: iconName, style_class: 'popup-menu-icon'}));
    box.add_child(new St.Label({
        text: label,
        y_align: Clutter.ActorAlign.CENTER,
    }));
    const button = new St.Button({
        style_class: 'keylightd-preset-tile',
        child: box,
        can_focus: true,
        x_expand: true,
    });
    button.connect('clicked', () => onClick());
    return button;
}

// Rate limiter with leading-edge and trailing-edge delivery. submit() coalesces
// rapid values into at most one apply() per interval; flush() delivers the last
// pending value immediately (used on drag-end).
class Throttle {
    constructor(intervalMs, apply) {
        this._interval = intervalMs;
        this._apply = apply;
        this._timer = 0;
        this._pending = null;
        this._hasPending = false;
    }

    submit(value) {
        this._pending = value;
        this._hasPending = true;
        if (this._timer)
            return;
        this._deliver();
        this._timer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, this._interval, () => {
            if (this._hasPending) {
                this._deliver();
                return GLib.SOURCE_CONTINUE;
            }
            this._timer = 0;
            return GLib.SOURCE_REMOVE;
        });
    }

    flush() {
        if (this._hasPending)
            this._deliver();
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
    }

    _deliver() {
        if (!this._hasPending)
            return;
        const value = this._pending;
        this._hasPending = false;
        this._apply(value);
    }

    destroy() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
        this._hasPending = false;
    }
}

// Per-light controls added to the button's own popup menu. A section header
// carries the light name, followed by a brightness row whose leading icon toggles
// power and whose trailing chevron expands an inline colour-temperature row.
class LightControls {
    constructor(proxy, menu) {
        this._proxy = proxy;
        this._updating = false;
        this._expanded = false;

        this._header = new PopupMenu.PopupSeparatorMenuItem();
        menu.addMenuItem(this._header);

        this._powerIcon = new St.Icon({
            icon_name: 'display-brightness-symbolic',
            style_class: 'popup-menu-icon',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._powerButton = new St.Button({
            child: this._powerIcon,
            style_class: 'keylightd-icon-button keylightd-slot',
            can_focus: true,
        });
        this._powerButton.connect('clicked', () => this._proxy.TogglePowerRemote());

        this._slider = new Slider(0);
        this._brightnessValue = new St.Label({
            style_class: 'keylightd-value-label',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._brightnessThrottle = new Throttle(SLIDER_THROTTLE_MS, (value) => {
            this._proxy.SetBrightnessRemote(brightnessFromSlider(value));
        });
        this._slider.connect('notify::value', () => {
            if (this._updating)
                return;
            this._brightnessValue.text = `${brightnessFromSlider(this._slider.value)}%`;
            this._brightnessThrottle.submit(this._slider.value);
        });
        this._slider.connect('drag-begin', () => (this._brightnessDragging = true));
        this._slider.connect('drag-end', () => {
            this._brightnessDragging = false;
            this._brightnessThrottle.flush();
        });

        this._chevronIcon = new St.Icon({
            icon_name: 'pan-end-symbolic',
            style_class: 'popup-menu-icon',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._chevron = new St.Button({
            child: this._chevronIcon,
            style_class: 'keylightd-icon-button keylightd-slot',
            can_focus: true,
        });
        this._chevron.connect('clicked', () => this._toggleExpanded());

        this._brightnessItem = new PopupMenu.PopupBaseMenuItem({activate: false});
        this._brightnessItem.add_style_class_name('keylightd-slider-item');
        this._brightnessItem.add_child(this._powerButton);
        this._brightnessItem.add_child(this._slider);
        this._brightnessItem.add_child(this._brightnessValue);
        this._brightnessItem.add_child(this._chevron);
        menu.addMenuItem(this._brightnessItem);

        this._tempIcon = new St.Icon({
            gicon: extensionIcon('keylightd-temperature-symbolic'),
            style_class: 'popup-menu-icon',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._tempLead = new St.Bin({
            child: this._tempIcon,
            style_class: 'keylightd-slot',
        });
        this._tempSlider = new Slider(0);
        this._temperatureValue = new St.Label({
            style_class: 'keylightd-value-label',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._temperatureThrottle = new Throttle(SLIDER_THROTTLE_MS, (value) => {
            this._proxy.SetTemperatureKelvinRemote(kelvinFromSlider(value));
        });
        this._tempSlider.connect('notify::value', () => {
            if (this._updating)
                return;
            const kelvin = this._currentKelvin();
            this._temperatureValue.text = `${kelvin} K`;
            this._tempIcon.style = `color: ${kelvinColor(kelvin)};`;
            this._temperatureThrottle.submit(this._tempSlider.value);
        });
        this._tempSlider.connect('drag-begin', () => (this._temperatureDragging = true));
        this._tempSlider.connect('drag-end', () => {
            this._temperatureDragging = false;
            this._temperatureThrottle.flush();
        });
        const spacer = new St.Widget({style_class: 'keylightd-slot'});
        this._tempItem = new PopupMenu.PopupBaseMenuItem({activate: false});
        this._tempItem.add_style_class_name('keylightd-slider-item');
        this._tempItem.add_child(this._tempLead);
        this._tempItem.add_child(this._tempSlider);
        this._tempItem.add_child(this._temperatureValue);
        this._tempItem.add_child(spacer);
        this._tempItem.visible = false;
        menu.addMenuItem(this._tempItem);

        this._proxy.connectObject('g-properties-changed', () => this.sync(), this);
        this.sync();
    }

    get proxy() {
        return this._proxy;
    }

    _currentKelvin() {
        return kelvinFromSlider(this._tempSlider.value);
    }

    _toggleExpanded() {
        this._expanded = !this._expanded;
        this._tempItem.visible = this._expanded;
        this._chevronIcon.icon_name = this._expanded ? 'pan-down-symbolic' : 'pan-end-symbolic';
    }

    sync() {
        this._updating = true;
        const name = this._proxy.Name ?? _('Key Light');
        const on = this._proxy.On ?? false;
        const brightness = clamp(this._proxy.Brightness ?? BRIGHTNESS_MIN, BRIGHTNESS_MIN, BRIGHTNESS_MAX);
        const kelvin = clamp(this._proxy.TemperatureKelvin ?? KELVIN_MIN, KELVIN_MIN, KELVIN_MAX);
        const reachable = this._proxy.Reachable ?? false;

        this._header.label.text = name;
        // Don't fight an in-progress drag by snapping the handle to the daemon's
        // value; the throttle already streams the user's intent.
        if (!this._brightnessDragging) {
            this._slider.value = brightness / BRIGHTNESS_MAX;
            this._brightnessValue.text = `${brightness}%`;
        }
        if (!this._temperatureDragging) {
            this._tempSlider.value = (kelvin - KELVIN_MIN) / (KELVIN_MAX - KELVIN_MIN);
            this._temperatureValue.text = `${kelvin} K`;
        }
        this._tempIcon.style = `color: ${kelvinColor(kelvin)};`;
        if (on)
            this._powerIcon.remove_style_class_name('keylightd-dim');
        else
            this._powerIcon.add_style_class_name('keylightd-dim');
        this._brightnessItem.setSensitive(reachable);
        this._tempItem.setSensitive(reachable);
        this._slider.reactive = reachable;
        this._tempSlider.reactive = reachable;
        this._updating = false;
    }

    destroy() {
        this._brightnessThrottle.destroy();
        this._temperatureThrottle.destroy();
        this._proxy?.disconnectObject(this);
        this._proxy = null;
        this._header.destroy();
        this._brightnessItem.destroy();
        this._tempItem.destroy();
    }
}

const KeylightdButton = GObject.registerClass(
class KeylightdButton extends PanelMenu.Button {
    _init(connection) {
        super._init(0.5, 'keylightd', false);
        this._connection = connection;
        this._lights = [];

        // Match the panel spacing of neighbouring status icons by wrapping the
        // icon in a panel-status-indicators-box, so the theme zeroes the per-icon
        // padding it otherwise applies to a status icon that is a direct panel child.
        this._panelIcon = new St.Icon({
            gicon: extensionIcon('keylightd-symbolic'),
            style_class: 'system-status-icon',
        });
        const iconBox = new St.BoxLayout({style_class: 'panel-status-indicators-box'});
        iconBox.add_child(this._panelIcon);
        this.add_child(iconBox);

        this._lightSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._lightSection);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        // Two always-visible preset tiles, side by side, mirroring the system
        // Quick Settings tiles, where save captures the current look and apply
        // restores it.
        this._save = makePresetTile('document-save-symbolic', _('Save preset'),
            () => this._root?.SavePresetRemote());
        this._apply = makePresetTile('media-playback-start-symbolic', _('Apply preset'),
            () => this._root?.ApplyPresetRemote());

        const tiles = new St.BoxLayout({style_class: 'keylightd-preset-tiles', x_expand: true});
        tiles.add_child(this._save);
        tiles.add_child(this._apply);

        const presetItem = new PopupMenu.PopupBaseMenuItem({
            activate: false,
            reactive: false,
            can_focus: false,
        });
        presetItem.add_style_class_name('keylightd-preset-item');
        presetItem.add_child(tiles);
        this.menu.addMenuItem(presetItem);

        this._root = RootProxy(connection, BUS_NAME, ROOT_PATH, (_proxy, error) => {
            if (error) {
                console.error(`keylightd: root proxy failed: ${error}`);
                return;
            }
            this._buildLights();
            this._syncRoot();
        });
        this._root.connectObject('g-properties-changed', () => this._syncRoot(), this);
    }

    _buildLights() {
        const paths = this._root.LightPaths ?? [];
        for (const path of paths) {
            const proxy = LightProxy(this._connection, BUS_NAME, path, (_proxy, error) => {
                if (error)
                    console.error(`keylightd: light proxy ${path} failed: ${error}`);
            });
            this._lights.push(new LightControls(proxy, this._lightSection));
        }
    }

    _syncRoot() {
        const active = this._root?.CameraActive ?? false;
        if (active)
            this._panelIcon.add_style_class_name('keylightd-camera-active');
        else
            this._panelIcon.remove_style_class_name('keylightd-camera-active');
        const hasPreset = this._root?.HasPreset ?? false;
        this._apply.reactive = hasPreset;
        if (hasPreset)
            this._apply.remove_style_class_name('keylightd-dim');
        else
            this._apply.add_style_class_name('keylightd-dim');
    }

    adjustBrightness(delta) {
        for (const light of this._lights)
            light.proxy.AdjustBrightnessRemote(delta);
    }

    togglePower() {
        for (const light of this._lights)
            light.proxy.TogglePowerRemote();
    }

    destroy() {
        this._root?.disconnectObject(this);
        this._root = null;
        for (const light of this._lights)
            light.destroy();
        this._lights = [];
        super.destroy();
    }
});

export default class KeylightdExtension extends Extension {
    enable() {
        iconsDir = `${this.path}/icons`;
        this._settings = this.getSettings();
        this._button = null;
        this._addKeybindings();
        this._watchId = Gio.bus_watch_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            (connection) => this._onAppeared(connection),
            () => this._onVanished());
    }

    disable() {
        this._removeKeybindings();
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }
        this._onVanished();
        this._settings = null;
        iconsDir = null;
    }

    _onAppeared(connection) {
        if (this._button)
            return;
        this._button = new KeylightdButton(connection);
        Main.panel.addToStatusArea('keylightd', this._button, 0, 'right');
    }

    _onVanished() {
        this._button?.destroy();
        this._button = null;
    }

    _addKeybindings() {
        const mode = Shell.ActionMode.NORMAL | Shell.ActionMode.OVERVIEW;
        Main.wm.addKeybinding('brightness-up', this._settings, Meta.KeyBindingFlags.NONE, mode,
            () => this._button?.adjustBrightness(BRIGHTNESS_STEP));
        Main.wm.addKeybinding('brightness-down', this._settings, Meta.KeyBindingFlags.NONE, mode,
            () => this._button?.adjustBrightness(-BRIGHTNESS_STEP));
        Main.wm.addKeybinding('toggle-power', this._settings, Meta.KeyBindingFlags.NONE, mode,
            () => this._button?.togglePower());
    }

    _removeKeybindings() {
        for (const key of KEYBINDINGS)
            Main.wm.removeKeybinding(key);
    }
}
