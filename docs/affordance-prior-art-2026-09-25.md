# Affordance channel prior art

Status: research input for the affordances channel proposed in
[#32](https://github.com/flotilla-org/jackstay/issues/32). No decision is
recorded here. The framing under test: a producer publishes typed state and
accepts typed commands with per-verb capability flags; the host renders all
chrome; the producer never sends chrome pixels, layout or styling. This note
extracts what existing systems that follow that split actually carry, so a v1
vocabulary can be sized from evidence. Sources are primary; unconfirmed items
are marked **UNVERIFIED**.
Researched 2026-09-25 against primary sources (protocol XML fetched from upstream git, vendor API reference pages, W3C ED, UPnP PDF). Items not confirmed from a primary source are marked **UNVERIFIED**.

## Summary

Every system surveyed has the same four parts. (1) A **small fixed verb set**: roughly 5–10 transport verbs for media, 5 for navigation, about 8 for windows, 2–4 for menus. (2) **Typed properties** that the host reads and redraws itself. (3) **Per-verb capability flags** (MPRIS `CanX`, SMTC `IsXEnabled`, Cast `supportedMediaCommands` bitmask, UPnP `CurrentTransportActions`, xdg `wm_capabilities`, Media Session "has a handler"). Hosts use these flags to hide or disable controls before anyone presses them. (4) **Change notification** by property-changed signals. Continuous values like playback position are the exception: they are sent as *(value, rate, timestamp)* and the host extrapolates, instead of pushing a stream of ticks. None of them sends geometry, layout, styling or pixels for chrome. The spec either says outright that presentation belongs to the other side ("You don't have direct control over what information the system displays, or its formatting": Apple), or makes the producer's request only a *preference* ("There are no guarantees as to what menu items the window menu contains, or even if a window menu will be drawn at all": xdg-shell `show_window_menu`). The long tail (ratings, likes, chapters, camera and mic toggles, downloads, permissions) is where each system grew once v1 had shipped. Every system handles extension either with an optional interface or bitmask bit or with a vendor prefix (`x-<vendor>-`, `customData`, `since=` versions).

---

## A. Media transport controls

### A1. MPRIS 2.2 (`org.mpris.MediaPlayer2.*`)
Sources: spec index https://specifications.freedesktop.org/mpris/latest/ ; XML at https://gitlab.freedesktop.org/mpris/mpris-spec/-/tree/master/spec ; metadata keys https://www.freedesktop.org/wiki/Specifications/mpris-spec/metadata/

| Interface | Methods | Properties (rw = writable) | Signals |
|---|---|---|---|
| `MediaPlayer2` (root) | Raise, Quit | CanQuit, CanRaise, Fullscreen (rw, opt), CanSetFullscreen (opt), HasTrackList, Identity, DesktopEntry (opt), SupportedUriSchemes, SupportedMimeTypes | via Properties.PropertiesChanged |
| `.Player` | Next, Previous, Pause, PlayPause, Stop, Play, Seek(Offset µs), SetPosition(TrackId, µs), OpenUri; *GetArtworkFd (opt)* | PlaybackStatus {Playing, Paused, Stopped}, LoopStatus {None, Track, Playlist} (rw, opt), Rate (rw), Shuffle (rw, opt), Metadata a{sv}, Volume (rw), Position, MinimumRate, MaximumRate, CanGoNext, CanGoPrevious, CanPlay, CanPause, CanSeek, CanControl | Seeked(µs) |
| `.TrackList` (opt) | GetTracksMetadata, AddTrack, RemoveTrack, GoTo | Tracks (EmitsChangedSignal=invalidates), CanEditTracks | TrackListReplaced, TrackAdded, TrackRemoved, TrackMetadataChanged |
| `.Playlists` (opt, 2.1+) | ActivatePlaylist, GetPlaylists; *GetPlaylistIconFd* | PlaylistCount, Orderings, ActivePlaylist | PlaylistChanged |

- **Metadata keys**: mpris:trackid, mpris:length, mpris:artUrl, plus xesam:{album, albumArtist, artist, asText, audioBPM, autoRating, comment, composer, contentCreated, discNumber, firstUsed, genre, lastUsed, lyricist, title, trackNumber, url, useCount, userRating}.
- **Capabilities**: `Can*` booleans. `CanControl` says: "If this is false, clients should assume that … all other properties starting with 'Can' are also false … This allows clients to determine whether to present and enable controls to the user in advance" (Player.xml `CanControl`). `CanGoNext` should be true when the outcome is unknown.
- **Change notification**: standard `org.freedesktop.DBus.Properties.PropertiesChanged`, annotated per property with `EmitsChangedSignal` (true/false/invalidates). **Position does not emit changes** (`EmitsChangedSignal=false`). Instead, `Seeked` fires only when "the track position has changed in a way that is inconsistant with the current playing state. When this signal is not received, clients should assume that: When playing, the position progresses according to the rate property" (Player.xml `Seeked`).
- **Versioning**: the spec version is in the document (2.0→2.1 added Playlists; 2.1→2.2 added optional Fullscreen). Optional members carry the annotation `org.mpris.MediaPlayer2.property.optional`. Optional interfaces are discovered via D-Bus introspection and `HasTrackList`. `GetArtworkFd`/`GetPlaylistIconFd` were added to master in Feb 2026 (commits "Added GetArtwork method", 2026-02-18) and do **not** appear on the published v2.2 page yet.
- **Excludes**: UI or window control beyond `Raise`/`Fullscreen`. The `Raise` doc says the player "may not have a graphical user interface at all". It also excludes equalizer and audio routing, library browsing (Playlists is optional and flat), and any layout or visual hints.

### A2. Apple MPRemoteCommandCenter / MPNowPlayingInfoCenter
Sources: https://developer.apple.com/documentation/mediaplayer/mpremotecommandcenter , …/mpnowplayinginfocenter , …/mpremotecommand/isenabled

- **Commands** (each is an `MPRemoteCommand` with `isEnabled` and `addTarget`): pause, play, stop, togglePlayPause; nextTrack, previousTrack, changeRepeatMode, changeShuffleMode; changePlaybackRate, seekBackward, seekForward, skipBackward, skipForward (`preferredIntervals`), changePlaybackPosition; rating, like, dislike; bookmark; enableLanguageOption, disableLanguageOption. That is 20 commands.
- **State**: the `nowPlayingInfo` dictionary. MPMediaItem subset: Title, Artist, AlbumTitle, AlbumTrackCount/Number, Artwork, Composer, DiscCount/Number, Genre, MediaType, PersistentID, PlaybackDuration. `MPNowPlayingInfoProperty*` (24 keys), notably ElapsedPlaybackTime, PlaybackRate, DefaultPlaybackRate, PlaybackProgress, PlaybackQueueCount/Index, ChapterCount/Number, IsLiveStream, MediaType, AssetURL, AvailableLanguageOptions/CurrentLanguageOptions, AdTimeRanges, CreditsStartTime. `playbackState` (macOS): unknown, playing, paused, stopped, interrupted.
- **Capabilities**: `MPRemoteCommand.isEnabled`. When false, "events for this command are not sent to your app, and the user interface may be changed to reflect this".
- **Change notification**: none. The app re-assigns the whole `nowPlayingInfo` dictionary. Elapsed time plus rate lets the system extrapolate. (The extrapolation is inferred from the key pairing and is **UNVERIFIED** as normative text.)
- **Versioning**: new keys and commands are added per OS release (availability annotations). There is no wire version.
- **Excludes**: "You don't have direct control over what information the system displays, or its formatting … the system or the connected accessory handles displaying the information in a consistent manner for all apps" (MPNowPlayingInfoCenter overview).

### A3. Windows SystemMediaTransportControls (WinRT)
Source: https://learn.microsoft.com/en-us/uwp/api/windows.media.systemmediatransportcontrols

- **Buttons** (`SystemMediaTransportControlsButton`): Play, Pause, Stop, Record, FastForward, Rewind, Next, Previous, ChannelUp, ChannelDown. All delivered through the single `ButtonPressed` event.
- **Other command events**: PlaybackPositionChangeRequested, PlaybackRateChangeRequested, ShuffleEnabledChangeRequested, AutoRepeatModeChangeRequested.
- **State**: PlaybackStatus (`MediaPlaybackStatus`: Closed, Changing, Stopped, Playing, Paused), PlaybackRate, ShuffleEnabled, AutoRepeatMode, SoundLevel (read). DisplayUpdater: Type, AppMediaId, Thumbnail, Music/Video/ImageProperties, `Update()`. Timeline via `UpdateTimelineProperties`: StartTime, EndTime, MinSeekTime, MaxSeekTime, Position.
- **Capabilities**: `IsEnabled` plus `Is{Play,Pause,Stop,Record,FastForward,Rewind,Next,Previous,ChannelUp,ChannelDown}Enabled`.
- **Notification**: the producer pushes by setting properties or calling `DisplayUpdater.Update()` / `UpdateTimelineProperties`. `PropertyChanged` exists for the SoundLevel direction. MinSeekTime and MaxSeekTime give a seekable window, which suits live content.
- **Excludes**: layout and visuals. The shell owns the flyout.

### A4. Google Cast media namespace (`urn:x-cast:com.google.cast.media`), with a brief UPnP AVTransport:1 note
Source: https://developers.google.com/cast/docs/media/messages

- **Commands** (JSON `type`): LOAD, PLAY, PAUSE, SEEK, STOP, GET_STATUS, VOLUME (stream volume). Queue and edit-tracks messages exist beyond this page.
- **MediaStatus**: mediaSessionId, media (MediaInformation, sent only when changed), playbackRate, playerState {IDLE, PLAYING, BUFFERING, PAUSED}, idleReason {CANCELLED, INTERRUPTED, FINISHED, ERROR}, currentTime, supportedMediaCommands, volume {level, muted}. MediaInformation: contentId, streamType {NONE, BUFFERED, LIVE}, contentType, metadata (Generic, Movie, TvShow, MusicTrack, Photo).
- **Capabilities**: `supportedMediaCommands` bitmask. 1<<0 Pause, 1<<1 Seek, 1<<2 Stream volume, 1<<3 Stream mute, 1<<4/5 Skip fwd/back (deprecated), 1<<6 Queue next, 1<<7 Queue prev, 1<<8 Queue shuffle, 1<<9 Skip ad, 1<<10 Repeat all, 1<<11 Repeat one, 1<<12 Edit tracks, 1<<13 Playback rate, 1<<14 Like, 1<<15 Dislike, 1<<16 Follow, 1<<17 Unfollow.
- **Notification**: the receiver broadcasts `MEDIA_STATUS` to all senders after each state change, and responses carry a `requestId`. Errors: INVALID_PLAYER_STATE, LOAD_FAILED, LOAD_CANCELLED, INVALID_REQUEST.
- **Extensibility**: `customData` on every message, and apps "may define [their] own messages" in custom namespaces. Messages are limited to 64 KB.
- **UPnP AVTransport:1** (https://upnp.org/specs/av/UPnP-av-AVTransport-v1-Service.pdf §2.2, §2.4). Actions: SetAVTransportURI, GetMediaInfo, GetTransportInfo, GetPositionInfo, GetDeviceCapabilities, GetTransportSettings, Stop, Play, Seek, Next, Previous (all required), and SetNextAVTransportURI, Pause, Record, SetPlayMode, SetRecordQualityMode, GetCurrentTransportActions (optional). §2.2.26 `CurrentTransportActions` is "a comma-separated list of transport-controlling actions that can be successfully invoked … at this specific point in time … used … to dynamically enable or disable play, stop, pause buttons". §2.2.27 `LastChange` is the single evented variable that batches (instance, var, value) changes. Positions are not evented; clients poll `GetPositionInfo`.

### A5. W3C Media Session (`navigator.mediaSession`)
Source: https://w3c.github.io/mediasession/ (§5 MediaSession, §4.4 Actions, §9 MediaPositionState)

- **Actions** (`MediaSessionAction`): play, pause, seekbackward, seekforward, previoustrack, nexttrack, skipad, stop, seekto, togglemicrophone, togglecamera, togglescreenshare, hangup, previousslide, nextslide, enterpictureinpicture, voiceactivity. That is 17. Details: seekOffset, seekTime, fastSeek, isActivating, enterPictureInPictureReason.
- **State**: `metadata` (title, artist, album, artwork[MediaImage{src, sizes, type}], chapterInfo[{title, startTime, artwork}]), `playbackState` {none, paused, playing}, `setPositionState({duration, playbackRate, position})`, and `setMicrophoneActive`/`setCameraActive`/`setScreenshareActive`.
- **Capabilities**: implicit. Registering a handler adds the action to the "supported media session actions", and `setActionHandler(a, null)` removes it.
- **Notification**: position is a snapshot (position, rate), and the UA extrapolates the "actual playback state" (§4.5). Metadata is replaced wholesale.
- **Excludes**: rendering. The abstract says the page shows "customized media metadata on platform UI, customize available platform media controls". §3.1 says the UA "is meant to use this information in any UI … either internal to the user agent or within the platform".

---

## B. Embedded web engine → embedder

### B1. WKWebView
Sources: https://developer.apple.com/documentation/webkit/wkwebview , …/wkuidelegate , …/wknavigationdelegate

| Kind | Members |
|---|---|
| Observable state (KVO) | url, title, isLoading, estimatedProgress (0..1), canGoBack, canGoForward, hasOnlySecureContent, serverTrust, themeColor, underPageBackgroundColor, mediaType, pageZoom, magnification, fullscreenState, cameraCaptureState, microphoneCaptureState. The docs state "KVO compliant" explicitly for url, title, isLoading and estimatedProgress; the rest are **UNVERIFIED** per page. |
| Navigation verbs | load(_:), loadHTMLString, loadFileURL, goBack(), goForward(), go(to:), reload(), reloadFromOrigin(), stopLoading() |
| WKUIDelegate (host-chrome hooks) | createWebViewWith (new window), webViewDidClose, runJavaScriptAlertPanel / ConfirmPanel / TextInputPanel, runOpenPanelWith (file upload), contextMenuConfigurationForElement / WillPresent / DidEnd, edit-menu present/dismiss, requestMediaCapturePermission, requestDeviceOrientationAndMotionPermission |
| WKNavigationDelegate | decidePolicyFor (action/response), didStartProvisionalNavigation, didReceiveServerRedirect, didCommit, didFinish, didFail / didFailProvisional, didReceive (auth challenge), webContentProcessDidTerminate, navigationAction/Response didBecome (download) |

- **Notification**: KVO on properties, plus delegate callbacks for events.
- **Excludes**: WebKit draws no back/forward, URL or tab chrome. Every dialog, menu, file picker and new window is delegated as a request carrying a completion handler, and the host must answer it.

### B2. Microsoft WebView2 `CoreWebView2`
Source: https://learn.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2 (view webview2-dotnet-1.0.4191.47)

- **Properties** (23 total; chrome-relevant): Source, DocumentTitle, CanGoBack, CanGoForward, ContainsFullScreenElement, FaviconUri, StatusBarText, IsDocumentPlayingAudio, IsMuted, IsSuspended, IsDefaultDownloadDialogOpen, WindowControlsOverlay.
- **Navigation verbs**: Navigate, NavigateToString, NavigateWithWebResourceRequest, GoBack, GoForward, Reload, Stop (plus about 37 other methods, for scripting, printing, downloads and devtools).
- **Events** (35; chrome-relevant): NavigationStarting, ContentLoading, SourceChanged, HistoryChanged, NavigationCompleted, DocumentTitleChanged, FaviconChanged, StatusBarTextChanged, ContainsFullScreenElementChanged, IsDocumentPlayingAudioChanged, IsMutedChanged, NewWindowRequested, WindowCloseRequested, ScriptDialogOpening, ContextMenuRequested, DownloadStarting, PermissionRequested, BasicAuthenticationRequested, ServerCertificateErrorDetected, ProcessFailed.
- **Notification**: one `XChanged` event per property. `HistoryChanged` covers CanGoBack/CanGoForward ("raised after SourceChanged and ContentLoading").
- **Host-draws-or-default split**: ContextMenuRequested: "The host has the option to create their own context menu with the information provided in the event or can add items to or remove items from WebView context menu. If the host doesn't handle the event, WebView will display the default context menu." The same pattern applies to DownloadStarting and the default download dialog.
- **Versioning**: COM interfaces `ICoreWebView2_N` plus runtime **feature detection** (QueryInterface) (https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/versioning).

### B3. Chromium `content::WebContentsDelegate` (optional)
Source: https://chromium.googlesource.com/chromium/src/+/main/content/public/browser/web_contents_delegate.h , invalidate_type.h

The hooks include OpenURLFromTab, AddNewContents, ActivateContents, CloseContents, LoadingStateChanged, UpdateTargetURL (hover status), SetContentsBounds, ContentsZoomChange, SetFocusToLocationBar, BeforeUnloadFired, CanDownload, HandleContextMenu, HandleKeyboardEvent, GetJavaScriptDialogManager, RunFileChooser, Enter/ExitFullscreenModeForTab, RequestMediaAccessPermission. The notable pattern is `NavigationStateChanged(source, InvalidateTypes changed_flags)`. The engine sends **dirty bits** (URL, TAB [favicon/crash], LOAD, TITLE, AUDIO), and the embedder then pulls the current values.

---

## C. Wayland cursor and window affordances

### C1. `wp_cursor_shape_v1` (staging, interface version 2)
Source: https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/cursor-shape/cursor-shape-v1.xml

- **Requests**: manager `get_pointer`, `get_tablet_tool_v2`. Device `set_shape(serial, shape)`, `destroy`. There are **no events** and no list of supported shapes (MR !289, "Does not advertises the list of supported cursors").
- **Shapes: 36** (values 1–36). default, context_menu, help, pointer, progress, wait, cell, crosshair, text, vertical_text, alias, copy, move, no_drop, not_allowed, grab, grabbing, e/n/ne/nw/s/se/sw/w_resize, ew/ns/nesw/nwse_resize, col_resize, row_resize, all_scroll, zoom_in, zoom_out. v2 adds dnd_ask and all_resize ("non-css value"). The names are "taken from the CSS W3C specification … with a few additions".
- **Why shapes, not pixels**: the XML only says "enumerated cursors instead of a wl_surface". The rationale is in issue #58 "Cursor shapes, server-side cursor themes" (https://gitlab.freedesktop.org/wayland/wayland-protocols/-/issues/58). Each client redundantly loads a theme ("as much as 8 MB extra per client"), theme and size are inconsistent across apps, live theme changes are handled badly, and HiDPI, magnifiers and multi-seat are affected. The issue concludes: "ultimately the compositor knows best." The protocol was upstreamed from Chromium's cursor-shapes-unstable-v1 (MR !194).
- **Versioning**: an enum `since="2"`. Unknown values raise a protocol error, so producers must gate on the bound version.

### C2. `xdg-decoration-unstable-v1` (version 2)
Source: …/unstable/xdg-decoration/xdg-decoration-unstable-v1.xml

- The manager has `get_toplevel_decoration`. The decoration object has requests `set_mode(mode)`, `unset_mode`, `destroy` and the event `configure(mode)`. Modes are `client_side=1` and `server_side=2`.
- **Semantics**: the client states a *preference*. "The compositor can decide not to use the client's mode and enforce a different mode instead", and a configure "must be obeyed". Without negotiation, "clients continue to self-decorate". A decoration is "a set of window controls as deemed appropriate by the party managing them".
- **Design discussion**: Simon Ser's cover letter (wayland-devel, 2018-02-18, https://lists.freedesktop.org/archives/wayland-devel/2018-February/037119.html) says the protocol is inspired by KDE's `server-decoration` and was iterated among Sway/wlroots, KDE and Mir. The protocol does not let the client describe *which* buttons or layout the server draws; that is left entirely to "the party managing them".

### C3. `xdg_toplevel` (xdg-shell, version 7) chrome affordances
Source: …/stable/xdg-shell/xdg-shell.xml

- **Requests**: set_parent, set_title, set_app_id, show_window_menu(seat, serial, x, y), move, resize(edges), set_max_size, set_min_size, set_maximized/unset_maximized, set_fullscreen(output)/unset_fullscreen, set_minimized.
- **Events**: configure(w, h, states[]), close, configure_bounds (v4), wm_capabilities (v5).
- **States**: maximized, fullscreen, resizing, activated, tiled_{left,right,top,bottom} (v2), suspended (v6, "not ordinarily being repainted … occluded … screen locking"), constrained_{left,right,top,bottom} (v7).
- **`wm_capabilities`** (v5): window_menu, maximize, fullscreen, minimize. "If a capability isn't supported, clients should hide or disable the UI elements that expose this functionality … The compositor will ignore requests it doesn't support." It is resent on change and followed by a configure.
- **`show_window_menu`**: "There are no guarantees as to what menu items the window menu contains, or even if a window menu will be drawn at all."
- **`xdg-toplevel-icon-v1`** (staging): `create_icon` gives an icon that takes `set_name` (a themed icon name) and/or `add_buffer(buffer, scale)`, then `set_icon(toplevel, icon)`. The compositor sends `icon_size`/`done` hints. "It is up to compositor policy whether to prefer using a buffer or loading an icon via its name." That means named-or-pixels, and the host chooses.

---

## D. Menus as data

### D1. `com.canonical.dbusmenu`
Source: libdbusmenu `libdbusmenu-glib/dbus-menu.xml` (https://git.launchpad.net/libdbusmenu/plain/libdbusmenu-glib/dbus-menu.xml)

- **Item model**: `(id:int, props:a{sv}, children:av)`, recursive. "A property should only be returned if its value is not the default value."
- **Item properties (default)**: type ("standard" \| "separator"), label (`_` mnemonic), enabled (true), visible (true), icon-name, icon-data (PNG), shortcut (`[["Control","S"]]`, chords allowed), toggle-type ("checkmark" \| "radio" \| ""), toggle-state (0, 1, other = indeterminate; default -1), children-display ("submenu"), disposition (normal \| informative \| warning \| alert).
- **Menu properties**: Version, TextDirection (ltr/rtl), Status (normal/notice), IconThemePath.
- **Methods**: GetLayout(parentId, recursionDepth, propertyNames) → (revision, layout); GetGroupProperties; GetProperty (debug only); Event(id, eventId ∈ {clicked, hovered, opened, closed}, data, timestamp); EventGroup; AboutToShow(id) → needUpdate; AboutToShowGroup.
- **Signals**: ItemsPropertiesUpdated(updated, removed), LayoutUpdated(revision, parent) (parent 0 means everything is invalid), ItemActivationRequested(id, ts).
- **Extensibility**: "Vendor specific types / properties / events can be added by prefixing them with `x-<vendor>-`."
- **Excludes**: geometry, fonts, colors, positions and widgets. Rendering belongs to the "applet". Radio-group consistency "is up to the toolkit wrappers". The spec has no text explaining *why* layout is absent; it is simply not modelled (**UNVERIFIED** as a stated rationale).

### D2. macOS Accessibility menus (brief)
Roles: `menuBar`, `menuBarItem`, `menu`, `menuItem`, `menuButton` (NSAccessibility.Role). Attributes: title, enabled, selected, `kAXMenuItemCmdCharAttribute` ("primary key in the keyboard shortcut"), `kAXMenuItemCmdModifiersAttribute`, `kAXMenuItemMarkCharAttribute`. Actions: `kAXPressAction`, `kAXShowMenuAction`. Notifications: `kAXMenuOpenedNotification`. Sources: https://developer.apple.com/documentation/applicationservices/kaxmenuitemcmdcharattribute and siblings. The shape is the same as dbusmenu (tree + label/enabled/shortcut/mark + press), but it is read-only introspection and not a publishing protocol.

---

## E. Scroll extents as state: UIA `IScrollProvider`
Sources: https://learn.microsoft.com/en-us/windows/win32/api/uiautomationcore/nn-uiautomationcore-iscrollprovider ; https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-implementingscroll

| Member | Meaning |
|---|---|
| HorizontallyScrollable / VerticallyScrollable | capability booleans |
| HorizontalScrollPercent / VerticalScrollPercent | position 0–100, or `UIA_ScrollPatternNoScroll` (-1) when not scrollable |
| HorizontalViewSize / VerticalViewSize | viewport as a % of content (100 when not scrollable) |
| `Scroll(horizAmount, vertAmount)` | ScrollAmount: LargeDecrement, SmallDecrement, NoAmount, LargeIncrement, SmallIncrement (page and arrow semantics) |
| `SetScrollPercent(h, v)` | absolute jump; pass NoScroll for an axis you don't care about |

- The guidelines say values are normalized to 0–100. When not scrollable, set 100/-1 "to avoid a race condition". Horizontal percent is **locale-relative** (100 means leftmost in RTL). Scrollbars themselves expose RangeValue, not Scroll. "This control pattern has no associated events". Changes arrive through generic UIA property-changed events.
- On macOS, `NSAccessibility.Attribute.horizontalScrollBar`/`verticalScrollBar` sit on the `scrollArea` role, and the scrollbar role carries `value` (0..1), orientation and min/maxValue. The exact value range is **UNVERIFIED**.

---

## F. Where the line was drawn

The per-system "Excludes" bullets above hold the stated boundaries, as quotes. In short: MPRIS covers "basic control over what is currently playing" and the player "may not have a graphical user interface at all". dbusmenu is a property bag plus a tree with no geometry and an `x-<vendor>-` hatch. xdg-decoration lets the client only *prefer* a two-value mode, and the button set is left "as deemed appropriate by the party managing them". cursor-shape uses semantic names that cannot be queried. Apple says: "You don't have direct control over what information the system displays, or its formatting." WebView2 lets the host draw its own UI or leave the engine default.

**Terminal precedent** (xterm ctlseqs, https://invisible-island.net/xterm/ctlseqs/ctlseqs.txt, "OSC Ps ; Pt ST"):
- OSC 0 sets icon name and title, OSC 1 sets the icon name, OSC 2 sets the title. Each is a string property, write-only, with no notification. (Title *reporting* via `CSI 21 t` exists but is often disabled for security. **UNVERIFIED** in this pass.)
- OSC 22 (xterm) is "Change pointer cursor shape to Pt … If Pt is empty, or does not match any of the standard names, xterm uses the … default." It is the named-cursor idea inside a byte stream. kitty extends it with a query: `OSC 22 ; ?pointer,crosshair,no-such-name ST` → `1,1,0` (https://sw.kovidgoyal.net/kitty/pointer-shapes/). That is a capability probe, which Wayland cursor-shape lacks.
- OSC 8 hyperlinks (https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda) use `OSC 8 ; params ; URI ST`. params is `key=value` joined by `:`, only `id` is defined, and "These parameters allow future extendability". Graceful degradation: "even if explicit hyperlinks aren't supported, the target URI is silently ignored".

---

## Common shape across systems

1. **Capability flags per verb**, separate from state. Examples: MPRIS `Can*`, SMTC `Is*Enabled`, Cast bitmask, UPnP `CurrentTransportActions`, Apple `isEnabled`, xdg `wm_capabilities`, UIA `*Scrollable`, dbusmenu `enabled`/`visible`, and in Media Session the presence of a handler. The stated purpose is always the same: let the host hide or disable controls *before* the user presses them. Flags change at runtime (per track or per page).
2. **Typed properties plus change signals.** There are three styles: per-property signals (D-Bus PropertiesChanged, WebView2 `*Changed`, KVO), dirty bits that the host follows with a pull (Chromium `InvalidateTypes`, dbusmenu `LayoutUpdated(revision)`), and whole-snapshot replacement (Apple nowPlayingInfo, Cast MEDIA_STATUS, Media Session metadata).
3. **Continuous values are not streamed.** Position travels as (position, rate) and is re-sent only when inconsistent (MPRIS `Seeked`, Media Session `setPositionState`, Apple elapsed+rate, Cast currentTime+playbackRate). UPnP makes clients poll.
4. **A small closed verb set** of about 5–10, plus one generic extension hatch (`customData`, `x-vendor-`, optional interfaces, `since=` enum values, OSC params).
5. **Requests, not commands, for host-owned UI.** Dialogs, new windows, context menus and file choosers are events carrying a reply path (WKUIDelegate completion handlers, WebView2 deferrals, Chromium delegate). Window-state changes are preferences that the other side can refuse (xdg).
6. **What none of them do**: send chrome pixels, layout coordinates, fonts or colors (themeColor is the lone hint). None let the producer dictate *how* a control looks, and none send widget trees beyond menus. The only pixels allowed are content-like assets (artwork, favicon, icon buffers), and even those are usually offered as name-or-URL-or-buffer with the host choosing.

---

## Union vocabulary

Key: MP=MPRIS, AP=Apple RemoteCommand/NowPlaying, SM=SMTC, GC=Cast, UP=UPnP AVT, MS=Media Session, WK=WKWebView, W2=WebView2, CR=Chromium WCD, XS=xdg-shell/decoration/icon, CS=cursor-shape, DM=dbusmenu, AX=macOS AX, UIA, T=terminal OSC.

### Media
| Item | Kind | Seen in |
|---|---|---|
| play / pause | cmd | MP AP SM GC UP MS |
| toggle play-pause | cmd | MP AP (MS via UA joint command) |
| stop | cmd | MP AP SM GC UP MS |
| next / previous | cmd | MP AP SM GC(queue) UP MS |
| seek absolute | cmd | MP(SetPosition) AP(changePlaybackPosition) SM GC UP MS(seekto) |
| seek relative / skip ±N | cmd | MP(Seek) AP(skip*, seek*) SM(FF/Rew) MS(seekfwd/back) |
| set rate | cmd/prop | MP AP SM GC |
| shuffle / repeat | prop | MP AP SM GC UP(PlayMode) |
| volume / mute | prop | MP GC |
| open URI / load | cmd | MP GC UP |
| playback status enum | prop | all six |
| position + duration | prop | all six (SM adds min/max seek) |
| metadata (title, artist, album, art) | prop | MP AP SM GC UP MS |
| capability per verb | flag | MP AP SM GC UP MS |
| like / dislike / rating / bookmark | cmd | AP GC (MP metadata only) |
| language / track options | cmd | AP |
| skip ad, chapters, live flag | cmd/prop | AP GC MS |
| record, channel up/down | cmd | SM UP |
| mic / camera / screenshare / hangup, slides | cmd | MS |
| tracklist / playlists | iface | MP (UP partly) |
| raise / quit / fullscreen | cmd | MP |

### Navigation
| Item | Kind | Seen in |
|---|---|---|
| url / source | prop | WK W2 CR |
| title | prop | WK W2 CR T(OSC 0/2) |
| canGoBack / canGoForward | flag | WK W2 |
| goBack / goForward / reload / stop / load | cmd | WK W2 |
| isLoading / progress | prop | WK(both) W2(events) CR(LoadingStateChanged) |
| secure-content / cert state | prop | WK W2 |
| favicon / theme color | prop | W2 WK(themeColor) CR |
| hover status text | prop | W2(StatusBarText) CR(UpdateTargetURL) |
| audio playing / muted | prop | W2 CR WK(mediaPlaybackState) |
| new window request | event | WK W2 CR |
| JS alert/confirm/prompt | event | WK W2 CR |
| context menu request | event | WK W2 CR |
| file chooser | event | WK CR (W2 via default UI) |
| download | event | WK W2 CR |
| permission request | event | WK W2 CR |
| fullscreen enter/exit | event | WK W2 CR |
| find in page, zoom | cmd | WK W2 CR |
| hyperlink on span | prop | T(OSC 8) |

### Window
| Item | Kind | Seen in |
|---|---|---|
| title, app id | prop | XS T |
| icon (name or buffer) | prop | XS(toplevel-icon) T(OSC 1) |
| min / max size | prop | XS |
| maximize / fullscreen / minimize | cmd | XS (MP fullscreen) |
| states (activated, resizing, tiled, suspended) | prop | XS |
| move / resize (interactive) | cmd | XS |
| window menu | cmd | XS |
| decoration owner | negotiation | XS |
| wm capabilities | flag | XS |
| close request | event | XS W2 CR |

### Cursor
| Item | Kind | Seen in |
|---|---|---|
| named shape (CSS set) | prop | CS T(OSC 22) |
| capability query for shapes | flag | T(kitty OSC 22 ?) only |

### Menu
| Item | Kind | Seen in |
|---|---|---|
| tree of items (id, children) | prop | DM AX |
| label (+ mnemonic), enabled, visible | prop | DM AX |
| icon, shortcut, toggle type/state, separator | prop | DM AX(cmd char/mark) |
| activate / clicked | cmd | DM AX |
| about-to-show (lazy fill) | cmd | DM |
| layout / props changed | event | DM AX(notifications) |

### Scroll
| Item | Kind | Seen in |
|---|---|---|
| scrollable h/v | flag | UIA AX |
| position % h/v | prop | UIA AX(scrollbar value) |
| viewport size % h/v | prop | UIA |
| scroll by small/large step | cmd | UIA |
| set position % | cmd | UIA |

**Intersection vs long tail**: across media, play/pause/stop/next/prev/seek, status, position+rate, core metadata and per-verb capability flags are universal. Rate, shuffle and repeat are common. Everything else sits in one or two systems. Across navigation, url/title/canGoBack/canGoForward/loading plus back/forward/reload/stop/load are universal. Dialog, menu and window requests are universal among engines, but every one of them uses a request/reply shape.

---

## Open questions

1. **Snapshot vs per-property deltas**: MPRIS, WebView2 and KVO send deltas; Apple, Cast and Media Session send snapshots; Chromium sends dirty bits and the host pulls. Which fits a channel that may be lossy or reconnecting?
2. **Capability model**: a per-verb bool, a bitmask (Cast), a list (UPnP, `wm_capabilities`), or implicit handler presence (Media Session)? And should unsupported verbs be *ignored* (xdg) or be *errors* (cursor-shape)?
3. **Host-owned UI requests** (dialogs, context menus, file pickers) need a reply channel with a deferral. Is v1 purely fire-and-forget, or does it include request/reply?
4. **Scroll units**: UIA uses percent (0–100, locale-relative horizontal) plus coarse steps. Is percent enough for a pixel-exact host scrollbar, or are content and viewport lengths needed?
5. **Named vs pixels for assets** (artwork, favicon, icon): every system permits pixels for *content* assets. Is that in scope for v1?
6. **Versioning**: per-interface integer (Wayland), optional-interface discovery (MPRIS), vendor prefixes (dbusmenu), or runtime feature detection (WebView2)?
7. **Menus**: is dbusmenu's lazy `AboutToShow` plus revisioned `LayoutUpdated` needed, or is a static menu snapshot enough for v1?
8. **UNVERIFIED items to confirm if they matter**: KVO compliance for WK themeColor, canGoBack and hasOnlySecureContent; the macOS AX scrollbar value range; xterm title reporting; Cast queue message names outside the messages page.
