# Multi-server + mIRC-style windows + detachable OS windows

## Context

Linefeed holds exactly one IRC connection and one OS window. The user is on several networks at once and wants to watch conversations side by side. Decisions made with the user:

- **Window model:** mIRC-style. Every status window, channel and query is a child window inside the main window. Child windows can be moved, resized, tiled, cascaded and maximized. A switchbar lists all windows, grouped by network.
- **Detach:** any child window can be detached into its own OS window and re-attached later.
- **Closing a window** works the same for child and OS windows. A channel window PARTs the channel. A query window closes the query. A status window disconnects that server, after a confirm.
- **Release:** one release with everything. The work still lands as a series of compilable, test-green commits on a branch.
- **Anti-goals:**
  - Existing settings.json and log folders must carry over with no manual fixes.
  - Windows tray behaviour must keep working with several OS windows open: minimize-to-tray, taskbar flash, balloon and single-instance activation.

Facts from exploration that shape the design:

- `IrcApp` (`src/gui/mod.rs:301-526`) mixes about 60 per-connection fields with global settings and single-view UI state.
- Every slash command reaches the server through the single `self.cmd_tx` and the implicit `self.current_channel`. That includes all 71 `send_command` sites in `commands.rs`.
- `LinefeedApp` (`src/main.rs:364`) has one `connection_thread` slot. `process_connection_lifecycle` (`main.rs:587-654`) drives one `ConnectionIntent`.
- These egui 0.37 APIs exist and were checked in the registry source:
  - `Context::show_viewport_immediate(id, builder, FnMut(&mut Ui, ViewportClass))` at context.rs:4662.
  - `Window::open/id/current_pos/default_rect/fixed_rect/constrain_to` at window.rs:162-448.
  - `Context::move_to_top` and `top_layer_id` at context.rs:3629 and 3645.
  - A bare `Context::default()` has `embed_viewports = true` (context.rs:797), so headless tests render a detached viewport as an embedded window.
- A child viewport is destroyed if it is not shown on a pass. eframe persists only the root window's geometry. `LinefeedApp::ui` returns early when minimized or tray-hidden (`main.rs:425-470`), which would kill detached viewports.
- The update-restart snapshot `UpdateSession` (`gui/mod.rs:60-69`) is written by the OLD binary and read by the NEW one (`updater/install.rs:174`). The new reader must accept the old single-session JSON.
- `ServerFavorite` (`gui/types.rs:38-74`) has 8 required serde fields. Neither `Settings` nor `ServerFavorite` uses `deny_unknown_fields`, so added `#[serde(default)]` fields are readable by 0.1.4.
- `systray.rs` finds the main window by exact title "Linefeed" (`systray.rs:52-94`). Detached windows must never use that exact title.

## Architecture: rename in place, extract the global state

The move with the least churn is to keep `IrcApp`'s per-connection fields and handlers where they are and rename the type to the per-connection **`Session`**. The global state, the UI state and the windows move out into a new **`App`**. Most of mod.rs's protocol code is already `self.<per-connection field>`, so it keeps compiling unchanged. Only the settings, UI and dialog code moves.

### `Session` (renamed `IrcApp`, stays in `src/gui/mod.rs` + `commands.rs`)

- **Keeps:**
  - Connection lifecycle: `connected`, `connecting`, `connection_intent`, `cmd_tx`, `msg_rx`, `closing_rx`, `active_session`, `pending_session`, and the reconnect counters.
  - Identity: `my_nick` plus the nick lineage.
  - Server state: `network_support`, `enabled_caps`, the chathistory and batch maps, `known_users`, `monitored_users`, and all `pending_*` maps.
  - Away and lag: away and auto-away state, lag fields, `channel_list*` data.
  - Buffers: `channels: HashMap<String, Channel>`, `server_messages`, `server_unread`.
  - Casemapped caches: `my_nick_lower`, `ignore_list_lower`, `highlight_words_lower`.
  - Handlers: all protocol handlers (`handle_incoming_message`, `handle_numeric`, `handle_cap_message`, `send_*`, `run_timed_tasks` minus global auto-away timing) and `process_command` for server commands.
- **New fields:**
  - `id: ConnectionId(u32)`, a newtype that is never reused.
  - `profile_id: Option<String>`.
  - `cfg: Rc<ConfigSnapshot>`: an immutable copy of the display and behaviour settings it reads, such as `max_scrollback`, `hide_join_part`, `ctcp_replies_enabled`, `part_message`, `quit_message` and the logging flags. It is replaced on every `save_settings`.
  - `log: Rc<LogManager>`: its methods already take `&self` behind a `RefCell`.
  - `events: VecDeque<SessionEvent>`.
- **Command routing:** `current_channel` stays on `Session` as "the target this command runs against". `App` sets it from the window the input came from before calling `process_input` or `process_command`. The ~180 `self.current_channel` reads and `add_message_to_current` keep working unchanged.
- **`SessionEvent`** has these variants: `TargetOpened(String)`, `TargetClosed(String)`, `TargetRenamed{old,new}`, `Activate(String)`, `Notify{title,body,force}`, `ShowChannelList`, `ShowChannelInfo(String)`, `StateChanged`.
  - Today's direct UI side effects become pushes onto this queue: setting `current_channel` on own JOIN, `show_channel_list = true` on RPL_LISTSTART, `send_notification`, and opening query tabs.
  - `App` drains the queue every frame. Session has no back-reference to App.
- **Focus and unread:** `window_focused` is replaced by an `Rc<Cell<FocusState>>` that `App` writes each frame, holding which target is active in a focused OS window. Unread and auto-read logic in `add_message_to_channel` (`mod.rs:5980`) asks it whether "this target is visible and focused".

### `App` (new `src/gui/app.rs`, implements the frame `ui`)

- **Owns:**
  - `sessions: Vec<Session>`.
  - The settings-backed global fields: Quick Connect form fields, `server_favorites`, `ignore_list`, display and behaviour settings, auto-away settings.
  - `updater`, `command_history`, `last_activity`, `last_notification_time`.
  - The dialog flags and dialog state: Connect, Settings, Channel List, Channel Info, colour picker.
  - The `WindowManager`.
- **Moves from `IrcApp` to `App`:**
  - `get_settings` and `save_settings`.
  - The `session_from_form` validation.
  - `send_notification`.
  - The keyboard shortcuts.
  - Every `show_*_window` in `dialogs.rs`, now taking `(&mut App, session id)` where they need a session.
- **App-level commands:** `process_command` gets a pre-dispatch in `App` for commands that are not about one session: `/server -m`, `/settings`, `/ignore`, `/unignore`, `/perform`, `/window`. Everything else goes to the target's session.
- **`/amsg` and `/ame`** stay per-session.

### Identity and windows

- **`TargetId { conn: ConnectionId, target: Option<String> }`**, where `None` is the status window. Target names are compared with that session's `network_support.identifiers_equal`.
- **`WindowManager { windows: Vec<TargetWindow>, active: Option<TargetId>, mode: Maximized | Floating, z_dirty }`**.
- **`TargetWindow`** has three fields:
  - `target`.
  - `placement`: either `Child { rect: Option<Rect>, pending_place: Option<Rect> }` or `Detached { pos: Option<Pos2>, size: Option<Vec2> }`.
  - `view: TargetView`. This holds `input_text`, `history_index`, `history_temp`, `tab_completion`, `selected_user`, `join_channel` and `scroll_to_bottom`, all moved off `IrcApp`.
- **Shared drawing:** `draw_target_view(ui, &mut Session, &mut TargetView, target)` draws the topic bar, scrollback, user list and input row. Child and detached windows both use it.
  - `draw_scrollback` (`mod.rs:6773`) gains a `target` parameter instead of reading `current_channel`.
  - Its ScrollArea `id_salt` becomes `("chat_scrollback", conn, target)`.
- **Row-height cache:** `RowHeightEntry` gets a two-slot cache, so one buffer drawn at two widths does not thrash. This only matters if the same target is ever drawn twice. Normally it is not, because a target is either child or detached.

### Child windows (mIRC MDI)

- **Main window layout:**
  - Top toolbar: active session's nick@endpoint, lag and away; Connect, Disconnect, Settings; a "Window" menu with Cascade, Tile, Maximize, Restore and New connection.
  - Below it, the switchbar. It is a horizontal wrapped strip grouped by session: `Libera: [Status] #rust #linux │ OFTC: [Status] #debian`. Unread colours match today's sidebar (`mod.rs:7325-7476`). It is sorted with `ordered_tabs`, so mouse order and keyboard order now agree.
  - The left channel sidebar is removed. The switchbar replaces it.
- **Maximized mode** is the default for upgraded users. Only the active window is drawn, filling the CentralPanel. Its window menu offers Restore, Detach and Close. This looks like today's app.
- **Floating mode:**
  - Each Child window uses `egui::Window::new(title).id(Id::new(("tw", conn, target))).open(&mut open).constrain_to(central_rect).default_rect(..).collapsible(false)`.
  - Tile and Cascade compute rects in `WindowManager`. They are applied once via `current_pos` plus `fixed_size` on the next frame through `pending_place`, then released.
  - The active window is tracked by a click or focus inside a window. That calls `ctx.move_to_top(layer)` and sets `active`. Switchbar clicks do the same.
- **Shortcuts:**
  - Ctrl+Tab and Ctrl+Shift+Tab cycle windows across sessions in switchbar order.
  - Alt+1..9 pick a window.
  - Ctrl+W closes the active window with the close semantics below.
  - Ctrl+M toggles maximize.
- **Close semantics** live in one function, `WindowManager::request_close(TargetId)`:
  - A joined channel calls `send PART` and stays open until the server confirms.
  - A query or unjoined channel calls `remove_channel`.
  - A status window opens a confirm dialog. Confirming calls `request_manual_disconnect`, then removes the session and all its windows. If it is the last session, the empty-state Connect dialog appears.
  - The existing `close_tab` (`mod.rs:6969`) logic is reused.
- **Window titles:** `"#rust @ Libera"`, where the network label is the profile name, else ISUPPORT `NETWORK`, else `host:port`.

### Detached OS windows

- **Drawing:** every frame, for each Detached window, call `ctx.show_viewport_immediate(ViewportId::from_hash_of(("detached", conn, target)), builder, |ui, _| …)`.
  - The builder is `ViewportBuilder::default().with_title("#rust @ Libera — Linefeed").with_inner_size(..).with_position(..).with_icon(..)`. The title never equals "Linefeed", so `systray::find_main_window` is unaffected.
  - Inside the callback: draw `draw_target_view`; read back `ui.ctx().input(|i| i.viewport().outer_rect/inner_rect/focused)` into the placement; handle `close_requested()` by calling `request_close`, which removes the window (and parts/closes).
  - Immediate was chosen over deferred because `Session` holds `Rc`/`Cell`, is `!Send`, and lives on the GUI thread. Deferred requires `Send + Sync + 'static`.
  - Cost is roughly one extra pass per open detached window per repaint. That is acceptable at the current adaptive 100-500 ms polling (`main.rs:517-553`).
- **Menus:** the child window menu and the switchbar right-click menu offer "Detach". The detached window has a "Reattach" button in its header row.
- **Root minimized:** `LinefeedApp::ui` stops returning early. It always pumps sessions, runs lifecycle, and draws detached viewports. It skips only the root panels while minimized.
- **Tray-hidden (Windows):**
  - `App` stops showing detached viewports, so egui destroys their OS windows. Geometry is already stored.
  - `systray::show_window` restore re-creates them.
  - Taskbar flash and balloon keep targeting the main HWND as today.
  - Single-instance activation keeps calling `show_window` and is unchanged.
- **Focus:** `app_focused = root focused || any detached viewport focused`. Notifications are suppressed only when the message's target is visible in a focused OS window.

## Settings, profiles, persistence, compatibility

- **Network profiles.** `ServerFavorite` gains `#[serde(default)] auto_connect: bool` and `#[serde(default)] id: String`. A random 16-hex id is assigned on load if empty, then saved.
  - The JSON key `server_favorites` and every currently required field stay the same. 0.1.4 settings load, and 0.1.4 can still read new files.
  - The Connect dialog's Favorites list gets an "Auto-connect at startup" checkbox per profile.
  - The legacy top-level server fields stay as the Quick Connect defaults.
- **Startup:** connect every profile with `auto_connect`. If there are none, show the Connect dialog exactly as today (`mod.rs:583`).
- **Connecting a new network:**
  - The Connect dialog's Connect button reuses the active session if it is disconnected and was never connected. Otherwise it opens a new session.
  - An explicit "Connect in new window" button always opens a new session.
  - `/server host` keeps its current meaning: it switches the current session, so existing tests and `pending_session` semantics stay.
  - `/server -m host [port]` opens a new session.
  - `SessionConfig` snapshots stay per session, so the credential-isolation invariant holds per session.
- **Layout persistence** goes in a new `layout.json` next to settings.json, written atomically the same way as `Settings::save`, with mode 0600. It holds the mode, active window, and per-target placement keyed by `(profile id or log_identity, canonical target)`.
  - It is saved debounced (2 s after a change) and on exit.
  - Keeping it separate means the credentials file is not rewritten on every window drag.
  - A missing or corrupt file means a maximized layout.
- **Update-restart snapshot:** introduce `UpdateSnapshot { sessions: Vec<UpdateSession>, layout: Option<Layout>, active: Option<…> }`.
  - The reader tries `UpdateSnapshot` first. It falls back to the old `UpdateSession` shape, which becomes a one-session snapshot. Both are tested.
  - `restore_update_session` (`mod.rs:1057`) runs per session.
- **Logs** are unchanged. Paths are already scoped by `SessionConfig::log_identity()` (`mod.rs:141`).
  - Two sessions on the same endpoint and nick would share files. Disallow that: opening a second session to an `EndpointKey` that is already live focuses the existing one, with a status message.

## Lifecycle and threading (`src/main.rs`)

- **Workers:** `connection_thread` becomes `workers: HashMap<ConnectionId, JoinHandle<()>>`.
  - `start_connection(ctx, id)` and `process_connection_lifecycle` loop over `app.sessions`. Each session keeps its own `ConnectionIntent`, reconnect backoff and closing grace.
  - The existing per-thread current-thread runtime and `run_worker_runtime` are unchanged.
- **Message pump:** `IrcApp::ui`'s drain moves to `App::pump`. It drains up to 100 messages per session per frame, then `update_minimal` per session.
- **Auto-away** idle timing moves to `App`, because user activity is global. It sends AWAY or BACK to every connected session.
- **Exit:** `on_exit` sends QUIT to all sessions (`prepare_update_shutdown` per session). It waits up to the same 2 s total deadline across all workers.
- **Session removal:** a session leaves `sessions` only after its worker has exited, or after the `closing_rx` grace has passed. That keeps the QUIT-flush behaviour from #243.

## Implementation steps (branch `feature/multi-server-windows`)

Each step compiles, passes `cargo test --locked`, and is clippy-clean.

0. **Design doc.** Commit `docs/superpowers/specs/2026-10-30-multi-server-windows-design.md`, which is this plan expanded.

1. **Split App/Session with one session, no behaviour change.**
   - Rename `IrcApp` to `Session` and add `gui/app.rs` with `App { sessions: vec![one], … }`.
   - Move the global, UI and dialog fields and their functions to `App`. Add `ConfigSnapshot`, `Rc<LogManager>` and `SessionEvent`, and convert the direct UI side effects into events.
   - `LinefeedApp.app` becomes `App`.
   - Tests:
     - Update mechanically. `test_app()` and `test_app_connected()` keep returning a `Session` built by `Session::with_settings`, with a test-only `ConfigSnapshot`.
     - Tests that touch dialogs or settings use `App`.
     - Add `session_events_reach_the_app_for_own_join_and_list_start`.

2. **Several sessions.**
   - Add `ConnectionId`, `workers` map, the per-session lifecycle loop and pump.
   - Add `/server -m`, the Connect dialog's two buttons, profile `id` and `auto_connect` plus startup auto-connect, and the duplicate-endpoint guard.
   - Make auto-away global, and add `UpdateSnapshot` with the old-format fallback.
   - The toolbar shows the active session. Keep the existing sidebar for now, grouped by session.
   - New tests:
     - main.rs, using two `scripted_server()`s: `two_sessions_register_independently_and_route_messages_to_their_own_buffers`; `disconnecting_one_session_leaves_the_other_connected`; `reconnect_backoff_is_tracked_per_session`.
     - mod.rs: `credentials_from_one_profile_never_reach_another_session`; `old_single_session_update_snapshot_restores_as_one_session`; `settings_from_0_1_4_load_and_new_fields_default`; `new_settings_file_deserializes_with_the_0_1_4_schema`, which uses a copy of the old struct in the test.

3. **Window manager and child windows.**
   - Add `TargetView` per window, with input, history and completion state moved off the session.
   - Add `draw_target_view` and the `draw_scrollback(target)` parameter.
   - Replace the sidebar with the switchbar. Add Maximized/Floating, Tile/Cascade, the shortcuts, `request_close` semantics with the status-close confirm, and `layout.json`.
   - New tests, all headless `ctx.run_ui`:
     - `closing_a_joined_channel_window_sends_part_and_keeps_it_until_confirmed`
     - `closing_a_status_window_asks_before_disconnecting`
     - `tile_places_every_floating_window_inside_the_central_rect_without_overlap`
     - `input_typed_in_one_window_is_sent_to_that_windows_session_and_target`
     - `switchbar_order_matches_ctrl_tab_order`
     - `layout_round_trips_and_a_corrupt_layout_falls_back_to_maximized`

4. **Detached viewports.**
   - Add `show_viewport_immediate`, geometry read-back and persistence, close handling, and Reattach.
   - Remove the root-minimized early return. Add tray-hidden suppression and re-creation, and the focus/unread/notification rules.
   - New tests, headless, where viewports embed:
     - `detached_window_renders_and_sends_from_its_own_input`
     - `detached_close_request_parts_the_channel`
     - `detached_windows_are_not_shown_while_tray_hidden_and_return_on_restore`
     - `unread_does_not_increase_for_a_target_visible_in_a_focused_detached_window`
   - Unit test the Win32 title check: `detached_window_titles_never_match_the_main_window_title`.

5. **Docs and QA.**
   - Update the README features and shortcuts, the CLAUDE.md architecture section and file sizes, and `docs/auto-updates.md` (multi-session restore). Add `docs/windows.md` describing the window model.

## Verification

- **After every step:**
  - `cargo check --locked && cargo test --locked && cargo clippy --all-targets -- -D warnings`
  - Windows cross-builds: `python3 build.py --arm64 --x64 --no-upx`, which also runs `update-deps --check`.
- **End-to-end, on the Linux build (X11 and Wayland):**
  - Connect Libera and OFTC from saved profiles with auto-connect. Restart and confirm both reconnect and the layout returns.
  - Tile and cascade. Detach `#rust` and move it to another monitor. Restart and confirm it reopens there; Wayland ignores position.
  - Close it and confirm it PARTs. Close OFTC's status window and confirm the prompt, the disconnect, and that Libera is unaffected.
  - Run an update-restart from 0.1.4 to the new build and confirm the old snapshot restores.
  - Downgrade to 0.1.4 and confirm settings.json still loads.
- **Windows manual QA**, on the ARM64 binary with minimize-to-tray on and a detached window open:
  - Minimize to tray and confirm all windows hide.
  - A PM while hidden shows a balloon.
  - Restore from the tray and confirm detached windows come back.
  - Launch a second instance and confirm it activates the first.
  - A highlight while unfocused flashes the taskbar.
- **macOS:** CI builds it only. Note the minimized-state caveat for manual checking (egui issue 3494).

## Risks

- **Step 1 is the large one.** It touches about 11.8k + 4.5k lines, mostly mechanically. Keep it free of behaviour changes so the existing 406 tests act as the safety net.
- **Removing the root-minimized early return** changes the CPU profile while minimized. Keep the 250 ms repaint cap when no detached window is open.
- **Wayland** cannot place windows, so detached-window positions are not restored there. Sizes still are.

## Implementation notes (what changed from this plan)

Recorded after building it, so the design and the code agree.

- **Each session keeps its own connect form.** The plan moved the Quick
  Connect form to `App`. Keeping it on `Session` matches mIRC (the Connect
  dialog edits the active status window), leaves `/server`'s form borrowing
  untouched, and kept ~200 test touches unchanged. `App.settings` is the
  canonical persisted state and only ever adopts a form that validates
  (`SessionEvent::SaveSettings`); config-only saves (`SaveConfig`,
  `SaveAutoPerform`) never touch the form, which carries the #316 rule over
  to several sessions. A transient read failure of settings.json at startup
  disables saving for the run instead of letting defaults overwrite it.
- **Behaviour settings are mirrored, not shared.** Instead of
  `Rc<ConfigSnapshot>`, sessions keep their existing fields and `App`
  pushes the canonical values with `Session::apply_config` (and adopts a
  session's own edits, e.g. `/ignore`, with `export_config`). The protocol
  code is unchanged.
- **Per-session `LogManager`.** Sessions to the same endpoint are refused,
  so log paths never collide and the shared `Rc` was unnecessary.
- **Window set is derived, not tracked.** `WindowManager::sync` rebuilds the
  window list from the sessions' tabs every frame, so no open/close events
  are needed; `SessionEvent` covers only notifications, settings, `/server -m`
  and `/window`.
- **Per-window input by swapping.** A window's input line, history position,
  completion and user selection (`TargetView`) are swapped into its session,
  with `current_channel` pointed at the window, while it draws or handles
  input (`app::with_window_view`), so the session's input code serves every
  window unchanged. Command history is shared by all windows.
- **Focus.** `Session::set_window_focused` (from #305) is driven per session:
  a session counts as focused only when one of our OS windows has focus and
  its window is the active one. Detached windows activate on focus or click.
- **Floating windows** get an explicit placement on their first frame (egui's
  first-frame sizing pass would otherwise give chat panels the whole screen),
  the constraint is skipped on the frame a placement is applied (it would use
  the previous size), and the active window is raised every frame so windows
  that open on their own never cover the one the user is typing in.
- **Notification policy** (focus, the setting, #323's separate PM clock and
  held summary) lives in `App::notify`; sessions only request notifications.
- **Startup focus** starts on the first network; later selection changes are
  followed only for the active network (#290 keeps unsolicited JOINs from
  moving focus at all).
- **Minimized main window.** eframe 0.37 runs only `App::logic` for a
  minimized or covered main window, unless one of its child viewports is
  visible: then it runs the ui pass too, without painting the main window.
  `logic` ticks every connection whenever the main window is not visible,
  and the ui pass that follows draws only the detached windows, which stay
  usable as planned. They hide (`Visible(false)`) only with the tray.
  Minimize-to-tray is driven from `logic` with a posted `ShowWindowAsync`:
  a synchronous `ShowWindow` or `GetWindowTextW` on our own window from
  inside eframe's event-loop callback deadlocks, which was what had looked
  like a hang in painting a minimized window.
- **Downgrades.** 0.1.4 reads settings written by this version but drops
  the fields it does not know when it saves. A favorite without an id gets
  one derived from its name, host and port (`derived_profile_id`), so its
  window placements in layout.json survive a downgrade and upgrade, and
  layout.json (which 0.1.4 never touches) keeps a copy of the auto-connect
  ticks by that id, restored for favorites that come back without one.
- **Snap layouts** (added after the plan, `snap.rs`): floating windows draw
  their own caption, so a drag can be followed for edge snaps and the
  Windows 11 layout bar; the maximize button opens the layout flyout, and
  Snap Assist offers the other windows for the zones left.
- **Dialogs follow the window that asked.** Channel info, the channel list
  and the colour picker open in the detached OS window they were requested
  from (`App::dialog_owner`, `App::color_target`), and so do Settings,
  Connect and the disconnect confirmation (`App::global_dialog_owner`, set
  when they open). The main window may be minimized, so nothing depends on
  bringing it forward; a detached status window being closed stays
  detached while it asks.
- **Floating windows follow the chat area.** `WindowManager::fit_to_area`
  scales attached placements when the central area changes (window resize,
  switchbar wrapping), and the area is saved in `layout.json` so a restart
  at another size rescales too.
