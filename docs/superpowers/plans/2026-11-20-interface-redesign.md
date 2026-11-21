# Linefeed Interface Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild Linefeed's main window to the approved design: a left conversation sidebar, one top bar, column-aligned message rows in a proportional font, a grouped people list, 44 px controls, dark and light themes, and Display settings that control them.

**Architecture:** A new `gui/theme.rs` owns every colour and size (a `Palette` and `Metrics` stored on the egui context) so drawing code asks for a role instead of a literal colour. A new `gui/fonts.rs` replaces `main.rs::setup_fonts` and adds a proportional UI font, a semibold face and a real monospace face. `App` (gui/app.rs) draws the top bar and sidebar; `Session` (gui/view.rs, gui/mod.rs) draws the header, rows, people list and composer from the palette.

**Tech Stack:** Rust 2024, egui/eframe 0.37 (no built-in egui fonts in this build: every glyph comes from system font files), inline `#[cfg(test)]` modules, frame tests via `egui::Context::run_ui`.

**Spec:** `docs/superpowers/specs/2026-11-20-interface-redesign/` (`Main.dc.html`, `Light.dc.html`, `Narrow.dc.html`, `Settings.dc.html`), the artboards of the approved canvas <https://claude.ai/artifact/PHa86bvaFqeRbBqKfERm4G>. Read the artboard a task names before starting it.

## Global Constraints

- Branch `ui-redesign`; one commit per task; never push or tag from this plan.
- Every commit passes: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and `cargo clippy --locked --all-targets --target aarch64-pc-windows-gnullvm -- -D warnings` (with `LLVM_MINGW_HOME=/opt/llvm-mingw-20260616-ucrt-ubuntu-22.04-aarch64` and its `bin` first on `PATH`).
- Tests never touch the real profile: `App::with_settings` only, no `App::load`.
- No new crates and no bundled font files. Fonts are loaded from system paths.
- Dark palette: bg `#1B1E23`, panel `#22262C`, raise `#2B3038`, line `#343A44`, text `#E4E7EB`, muted `#A3ABB8`, faint `#98A1AF`, accent `#6CA8FF`, on-accent `#0E1116`, mention `#F2A65A`, mention row `#37302A`, online `#5BC98C`, nicks `#7DB7FF #5ED0C0 #C39BFF #F28FB5 #B7D36B #F2A65A`.
- Light palette: bg `#FBFBFA`, panel `#F1F2F4`, raise `#E6E8EC`, line `#D5D9E0`, text `#1D2129`, muted `#4E5766`, faint `#5F6878`, accent `#1F5FD1`, on-accent `#FFFFFF`, mention `#A4500B`, mention row `#F7EBDD`, online `#1E8E5A`, nicks `#1F5FD1 #0B7268 #6B3FC4 #B0236B #4D6B0A #A4500B`.
- Active sidebar row: accent at 24 % over panel (dark), 14 % (light).
- Sizes: control height 44 px Comfortable / 32 px Compact (held in egui `spacing.interact_size.y`, read through `theme::control_height(ui)`); message row gap 6 / 0 px; control radius 10, list row radius 8, badge radius 11 with min width 22; sidebar 248 px (min 180, max 360); people list 208 px (min 140, max 35 % of the window); time column 48, nick column 108, column gap 12, row side padding 16.
- Text: body default 15 px for new installs (saved `font_size` values are kept; clamp stays 10–24), small = body × 0.85, heading = body × 1.2.
- Every text colour is at least 4.5:1 against the ground it is drawn on.
- Copy, exactly: `Join a channel`, `New messages`, `Enter sends. Shift+Enter starts a new line.`, `Message {target}`, `Search history (Ctrl+F)`, `{n} people`, `Match the system`, `Give each nickname its own color`, `Hide join, part and quit lines`, `Save & Close`.
- Narrow window: below 720 px inner width the sidebar is replaced by a switcher button in the top bar and the people list starts hidden; below 520 px of message width rows stack (nick and time above the text).
- Kept as is: window modes (maximized, floating, snap, detached), `ordered_windows()` order, shortcuts, context-menu entries, restore hold, every credential and logging invariant in CLAUDE.md.

## Review Focus

1. **IRC colour codes on the light theme.** White or yellow mIRC-coloured text with no background must still be readable; expected: nudged to at least 3:1 against the row ground. Test in Task 7.
2. **A channel with thousands of users.** The grouped people list must still build only the visible rows. Test in Task 8.
3. **Very long nicknames and channel names.** Sidebar rows, the nick column and the header truncate with an ellipsis and show the full name on hover; nothing widens a panel. Tests in Tasks 5 and 6.
4. **More conversations than fit.** The sidebar scrolls, and switching with Ctrl+Tab scrolls the active row into view. Test in Task 5.
5. **Missing font files.** On a system with none of the listed fonts the app starts with whatever `symbols` font exists, semibold falls back to regular, and nothing panics. Test in Task 2.

---

### Task 1: Theme tokens and display settings

**Files:**
- Create: `src/gui/theme.rs`
- Modify: `src/gui/mod.rs` (declare `mod theme;`, replace `style_with_font_size`/`apply_font_size`), `src/gui/types.rs` (`Settings`), `src/gui/app.rs` (`App::font_size` callers), `src/main.rs:395-398`
- Test: `src/gui/theme.rs`, `src/gui/types.rs`

**Interfaces:**
- Produces:
  - `pub(crate) enum ThemeChoice { Dark, Light, System }` (serde `"dark" | "light" | "system"`, default `Dark`)
  - `pub(crate) enum Density { Comfortable, Compact }` (default `Comfortable`)
  - `pub(crate) enum MessageFont { Proportional, Monospace }` (default `Proportional`)
  - `pub(crate) struct DisplayPrefs { pub theme: ThemeChoice, pub density: Density, pub message_font: MessageFont, pub font_size: f32, pub color_nicks: bool }` and `Settings::display(&self) -> DisplayPrefs`
  - `pub(crate) struct Palette { pub bg, panel, raise, line, text, muted, faint, accent, on_accent, mention, mention_row, online: Color32, pub nicks: [Color32; 6], pub dark: bool }` with `Palette::dark()`, `Palette::light()`, `fn nick(&self, nick: &str) -> Color32`, `fn active_row(&self) -> Color32`
  - `pub(crate) struct Metrics { pub control_height: f32, pub row_gap: f32, pub message_font: MessageFont, pub color_nicks: bool }`
  - `pub(crate) fn apply(ctx: &egui::Context, prefs: &DisplayPrefs)`, `pub(crate) fn palette(ctx: &egui::Context) -> Palette`, `pub(crate) fn metrics(ctx: &egui::Context) -> Metrics`
  - `pub(crate) fn contrast(a: Color32, b: Color32) -> f32` (WCAG ratio), `pub(crate) fn readable_on(color: Color32, ground: Color32, min: f32) -> Color32`
  - `Settings` gains `theme`, `density`, `message_font`, `color_nicks: bool` (default `true`); `font_size` default becomes `15.0`

- [ ] **Step 1: Write the failing tests**

```rust
// theme.rs
#[test]
fn every_text_role_reads_on_every_ground_it_is_drawn_on() {
    for p in [Palette::dark(), Palette::light()] {
        for ground in [p.bg, p.panel, p.raise, p.mention_row] {
            for text in [p.text, p.muted, p.faint] {
                assert!(contrast(text, ground) >= 4.5, "{text:?} on {ground:?}");
            }
        }
        for nick in p.nicks {
            assert!(contrast(nick, p.bg) >= 4.5 && contrast(nick, p.mention_row) >= 4.5);
        }
        assert!(contrast(p.on_accent, p.accent) >= 4.5);
        assert!(contrast(p.on_accent, p.mention) >= 4.5);
        assert!(contrast(p.accent, p.bg) >= 4.5 && contrast(p.mention, p.mention_row) >= 4.5);
    }
}
#[test]
fn applying_prefs_sets_visuals_sizes_and_the_stored_palette() {
    let ctx = egui::Context::default();
    apply(&ctx, &DisplayPrefs { theme: ThemeChoice::Light, density: Density::Compact,
        message_font: MessageFont::Monospace, font_size: 18.0, color_nicks: false });
    assert_eq!(palette(&ctx).bg, Color32::from_rgb(0xFB, 0xFB, 0xFA));
    assert!(!ctx.global_style().visuals.dark_mode);
    assert_eq!(ctx.global_style().spacing.interact_size.y, 32.0);
    assert_eq!(ctx.global_style().text_styles[&egui::TextStyle::Body].size, 18.0);
    assert_eq!(metrics(&ctx).row_gap, 0.0);
}
#[test]
fn a_context_nothing_was_applied_to_gets_the_dark_comfortable_defaults() {
    let ctx = egui::Context::default();
    assert_eq!(palette(&ctx).bg, Palette::dark().bg);
    assert_eq!(metrics(&ctx).control_height, 44.0);
}
#[test]
fn readable_on_leaves_good_colours_alone_and_fixes_bad_ones() {
    let p = Palette::light();
    assert_eq!(readable_on(p.text, p.bg, 3.0), p.text);
    assert!(contrast(readable_on(Color32::WHITE, p.bg, 3.0), p.bg) >= 3.0);
}
// types.rs
#[test]
fn settings_saved_by_0_1_9_load_with_the_new_display_defaults() {
    let s: Settings = serde_json::from_str(r#"{"font_size":14.0,"timestamp_format":"full"}"#).unwrap();
    assert_eq!((s.theme, s.density, s.message_font, s.color_nicks),
        (ThemeChoice::Dark, Density::Comfortable, MessageFont::Proportional, true));
    assert_eq!(s.font_size, 14.0);
    assert_eq!(Settings::default().font_size, 15.0);
}
```

- [ ] **Step 2:** Run `cargo test --locked theme:: settings_saved_by_0_1_9` and see them fail to compile.
- [ ] **Step 3:** Implement `theme.rs`. `apply` starts from `egui::Visuals::dark()`/`light()`, sets `panel_fill = panel`, `window_fill = panel`, `extreme_bg_color = bg`, `faint_bg_color = raise`, widget `bg_fill`/`weak_bg_fill` (`raise`), widget strokes (`line`, text colours), `selection.bg_fill = active_row()`, `hyperlink_color = accent`, corner radius 10; `spacing.interact_size.y = control_height`, `spacing.button_padding = (14, (control_height - body) / 2 clamped ≥ 4)`, `item_spacing = (8, 4)`; text styles Small/Body/Button/Monospace/Heading from `font_size`; stores `Palette` and `Metrics` with `ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("theme"), ..))`. `ThemeChoice::System` resolves through `ctx.system_theme()`, falling back to dark. `palette`/`metrics` return the dark, comfortable defaults when nothing is stored.
- [ ] **Step 4:** Replace `apply_font_size` callers (`main.rs`, `dialogs.rs:559`, tests at `mod.rs:9482`) with `theme::apply(ctx, &settings.display())`; delete `style_with_font_size`. Remove the `item_spacing` line in `main.rs`.
- [ ] **Step 5:** Run the full gate. Commit `Add the theme palette and display settings`.

### Task 2: Fonts

**Files:**
- Create: `src/gui/fonts.rs`
- Modify: `src/main.rs` (delete `setup_fonts`, call `gui::fonts::install`), `src/gui/mod.rs` (`pub mod fonts;`)
- Test: `src/gui/fonts.rs`

**Interfaces:**
- Produces:
  - `pub fn install(ctx: &egui::Context)`
  - `pub(crate) const SEMIBOLD: &str = "semibold"`; `pub(crate) fn semibold() -> egui::FontFamily` (`FontFamily::Name(SEMIBOLD.into())`)
  - `pub(crate) struct FontPlan { pub proportional: &'static [&'static str], pub semibold: &'static [&'static str], pub monospace: &'static [&'static str], pub symbols: &'static [&'static str], pub emoji: &'static [&'static str] }`, `pub(crate) fn plan() -> FontPlan`
  - `pub(crate) fn definitions(plan: &FontPlan, read: impl Fn(&str) -> Option<Vec<u8>>) -> egui::FontDefinitions`

Candidate paths, first readable wins: Windows proportional `segoeui.ttf`, semibold `seguisb.ttf` then `segoeuib.ttf`, monospace `consola.ttf`, symbols `seguisym.ttf`, emoji `seguiemj.ttf` (all under `C:\Windows\Fonts\`); macOS proportional `/System/Library/Fonts/SFNS.ttf` then `/System/Library/Fonts/Helvetica.ttc`, semibold the same list, monospace `/System/Library/Fonts/Menlo.ttc` then `Monaco.ttf`, emoji as today; Linux proportional `/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf` then `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf`, semibold `NotoSans-SemiBold.ttf`, `NotoSans-Bold.ttf`, `DejaVuSans-Bold.ttf`, monospace `DejaVuSansMono.ttf` then `NotoSansMono-Regular.ttf`, symbols and emoji as today.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn each_family_lists_its_own_face_then_the_shared_fallbacks() {
    let defs = definitions(&plan(), |_| Some(vec![0u8; 4]));
    let prop = &defs.families[&egui::FontFamily::Proportional];
    let mono = &defs.families[&egui::FontFamily::Monospace];
    let semi = &defs.families[&semibold()];
    assert_eq!((prop[0].as_str(), mono[0].as_str(), semi[0].as_str()), ("proportional", "monospace", "semibold"));
    for family in [prop, mono, semi] {
        assert!(family.contains(&"symbols".to_owned()) && family.contains(&"emoji".to_owned()));
    }
}
#[test]
fn a_system_with_no_fonts_still_defines_every_family() {
    let defs = definitions(&plan(), |_| None);
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace, semibold()] {
        assert!(defs.families.contains_key(&family));
    }
}
#[test]
fn a_missing_semibold_face_falls_back_to_the_proportional_one() {
    let plan = plan();
    let defs = definitions(&plan, |path| (!plan.semibold.contains(&path)).then(|| vec![0u8; 4]));
    assert_eq!(defs.families[&semibold()][0], "proportional");
}
```

- [ ] **Step 2:** Run `cargo test --locked fonts::` and see them fail.
- [ ] **Step 3:** Implement. `install` calls `definitions(&plan(), |p| std::fs::read(p).ok())` and `ctx.set_fonts`, logging each face loaded. Families that ended up empty get the first face that did load, so text is always drawable.
- [ ] **Step 4:** Run the full gate. Commit `Load a proportional UI font, a semibold face and a monospace face`.

### Task 3: Shared widgets

**Files:**
- Create: `src/gui/widgets.rs` (declare in `src/gui/mod.rs`)
- Test: `src/gui/widgets.rs`

**Interfaces:**
- Consumes: `theme::palette`, `theme::metrics`, `fonts::semibold`
- Produces:
  - `pub(crate) fn strong(text: impl Into<String>) -> egui::RichText` (semibold family, body size)
  - `pub(crate) fn badge(ui: &mut egui::Ui, text: &str, fill: Color32)` (radius 11, min width 22, `on_accent` text, small size)
  - `pub(crate) fn list_row(ui: &mut egui::Ui, selected: bool, add: impl FnOnce(&mut egui::Ui)) -> egui::Response` (full width, `control_height` tall, radius 8, `active_row()` fill when selected, `raise` on hover, click sense)
  - `pub(crate) fn truncated(ui: &mut egui::Ui, text: egui::RichText) -> egui::Response` (one line, ellipsis, full text on hover when cut)
  - `pub(crate) fn section_label(ui: &mut egui::Ui, text: &str)` (small, `muted`, upper-case)
  - `pub(crate) fn dot(ui: &mut egui::Ui, color: Color32, filled: bool)` (10 px; a 2 px ring when not filled)

- [ ] **Step 1: Write the failing tests** — frame tests on a bare `egui::Context` (after `theme::apply` with Comfortable): `list_row` response rect is the full available width and exactly 44 px tall, and 32 px after applying Compact; `truncated` with a 400-character string in a 120 px wide `Ui` returns a rect no wider than 120 px; `badge("12", ..)` is at least 22 px wide and paints the text `12`.
- [ ] **Step 2:** Run `cargo test --locked widgets::` and see them fail.
- [ ] **Step 3:** Implement with `ui.allocate_exact_size` + `ui.painter()` for `list_row`, `egui::Label::new(..).truncate()` for `truncated`.
- [ ] **Step 4:** Run the full gate. Commit `Add the redesign's shared row, badge and label widgets`.

### Task 4: Top bar

Spec: header of `Main.dc.html`; `Narrow.dc.html` for the narrow arrangement.

**Files:**
- Modify: `src/gui/app.rs` (`draw_toolbar`, `draw_toolbar_controls`, `ToolbarFit`, `toolbar_rows`, toolbar tests), `src/gui/view.rs` (`draw_status_summary`, `draw_account_security_status`; `draw_join_box` moves to Task 5)
- Test: `src/gui/app.rs`

**Interfaces:**
- Consumes: `widgets::*`, `theme::palette`
- Produces: `App::narrow(&self, ui: &egui::Ui) -> bool` (`ui.ctx().content_rect().width() < 720.0`), used by Tasks 5 and 8; `App::draw_connection_button(&mut self, ui)`.

Layout, left to right: connection button (dot coloured `online`/away amber/yellow connecting/red disconnected, network label in `strong`, nick in `muted`, chevron) whose menu holds `Connect` or `Disconnect`, `New connection...` and the detail lines of `draw_account_security_status`; one status line in `muted` small text (`Encrypted · signed in as {account} · {lag} ms`, or `Not encrypted` in `mention` colour for a plaintext link, or `Connecting to {label}...`, or `Disconnected`); right-aligned: search field 280 px (`Search history (Ctrl+F)`; Enter calls `open_search(active, query, false)`), `Activity` with a `mention`-coloured badge when unread items exist, `Window` menu (unchanged entries), `Settings`. The heading `Linefeed` is removed (it is the OS window title). `toolbar_rows` keeps its three arrangements with `heading` renamed `connection`.

- [ ] **Step 1: Write the failing tests** (replace the four existing toolbar tests):

```rust
#[test]
fn the_top_bar_puts_connection_left_and_search_activity_window_settings_right() {
    let (mut app, _a, _b) = two_networks();
    let rects = text_rects_at_width(&mut app, 1360.0);
    assert!(rect_of(&rects, "Settings").right() > 1360.0 - 24.0);
    let mut left = 680.0;
    for label in ["Activity", "Window", "Settings"] {
        let r = rect_of(&rects, label);
        assert!(r.left() > left, "{label}: {r:?}");
        left = r.right();
    }
    assert!(!rects.iter().any(|(t, _)| t == "Linefeed" || t == "Disconnect" || t == "Join"));
}
#[test]
fn disconnect_lives_in_the_connection_menu() { /* click the connection button's rect via RawInput pointer events, next frame paints "Disconnect" and "New connection..." */ }
#[test]
fn a_plaintext_connection_is_called_out_in_the_status_line() { /* TlsStatus::Plaintext → a painted text containing "Not encrypted" in palette.mention */ }
#[test]
fn every_top_bar_control_is_44_px_tall_and_32_when_compact() { /* widget rects of the buttons via ctx.read_response or painted background rects */ }
```
Keep `the_toolbar_arrangement_needs_slack_to_grow_but_none_to_shrink` with the renamed field, and `a_narrow_toolbar_wraps_inside_the_window` asserting every painted top-bar text stays inside 640 px.

- [ ] **Step 2:** Run `cargo test --locked top_bar toolbar` and see them fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full gate. Commit `Rebuild the top bar: connection button, search field, Activity, Window, Settings`.

### Task 5: Conversation sidebar

Spec: `<nav>` of `Main.dc.html`; the switcher button in `Narrow.dc.html`.

**Files:**
- Modify: `src/gui/app.rs` (`ui()` panel order; replace `draw_switchbar`, `switchbar_text`, `switchbar_label`), `src/gui/view.rs` (`draw_join_box` → sidebar join row), `src/gui/types.rs` (`Channel`)
- Test: `src/gui/app.rs`, `src/gui/types.rs`

**Interfaces:**
- Consumes: `widgets::{list_row, badge, truncated, section_label, dot}`, `App::narrow`
- Produces:
  - `Channel::unread_mentions(&self) -> usize` (highlighted messages among the last `unread`)
  - `App::draw_sidebar(&mut self, ui: &mut egui::Ui)`, `App::draw_conversation_rows(&mut self, ui: &mut egui::Ui)` (shared by the panel and the narrow switcher popup), `App::draw_switcher_button(&mut self, ui: &mut egui::Ui)`
  - `egui::Panel::left("sidebar")` shown before the central panel when `!narrow`; `default_size 248`, `min_size 180`, `max_size 360`, resizable; rows inside `ScrollArea::vertical()`.

Per network: `section_label` with a status dot, then one `list_row` per `session.ordered_tabs()` entry (`Status`, channel and query names; detached windows in italics with the existing hover text). Text is `text` + `strong` when active or unread, `muted` otherwise. Badge: `{unread}` on `accent`, or `@ {mentions}` on `mention` when `unread_mentions() > 0`; the Status row badges `server_unread`. Left click `user_activate` + `focus_detached`; the context menu keeps today's entries. After a connected network's rows: a `Join a channel` row that turns into the join text field (Enter joins through `send_join`, Escape cancels). When the active window changes, its row is scrolled into view (`scroll_to_me`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn unread_mentions_counts_highlights_among_the_unread_tail_only() { /* 5 messages, last 3 unread, highlights at index 0 and 4 → 1 */ }
#[test] fn the_sidebar_lists_every_window_in_ctrl_tab_order() { /* y-order of painted row labels == ordered_windows(); replaces switchbar_order_matches_ctrl_tab_order's drawing half */ }
#[test] fn a_maximized_frame_draws_the_active_window_and_the_sidebar() { /* rename of the switchbar test: "#rust" painted with x < 248 */ }
#[test] fn sidebar_badges_show_unread_counts_and_mark_mentions() { /* "#rust" with 3 unread → "3"; a highlighted unread → "@ 1" */ }
#[test] fn clicking_a_sidebar_row_activates_its_window() { /* pointer press+release at the row rect → windows.active */ }
#[test] fn a_long_channel_name_is_cut_inside_the_sidebar() { /* 200-char channel: label rect.right() <= panel right, panel width stays 248 */ }
#[test] fn the_active_row_is_scrolled_into_view() { /* 80 queries at height 400; activate the last; its label rect is inside the panel rect */ }
#[test] fn below_720_px_the_sidebar_becomes_a_switcher_button() { /* width 640: no row label at x < 248 except inside the top bar; the button shows the active target name and the summed unread */ }
#[test] fn join_a_channel_sends_a_join() { /* type "#new" + Enter in the join row → drained commands contain "JOIN #new" */ }
```

- [ ] **Step 2:** Run `cargo test --locked sidebar unread_mentions join_a_channel` and see them fail.
- [ ] **Step 3:** Implement; delete the switchbar functions and the `Panel::top("switchbar")` line; update CLAUDE.md's `switchbar` mentions to `sidebar`.
- [ ] **Step 4:** Run the full gate. Commit `Replace the switchbar with a conversation sidebar`.

### Task 6: Conversation header

Spec: the row under `<main>` in `Main.dc.html`.

**Files:**
- Modify: `src/gui/view.rs` (`draw_target_header`, `draw_target`), `src/gui/app.rs` (`draw_maximized` title row, `WindowAction`), `src/gui/windows.rs` (`TargetView`)
- Test: `src/gui/app.rs`

**Interfaces:**
- Produces: `TargetView.people_hidden: Option<bool>` (`None` = follow the width: hidden when `App::narrow`), swapped into `Session` as `self.people_hidden` while drawing; `Session::people_visible(&self, narrow: bool) -> bool`.

One row: target name as heading in `strong`; topic through `render_irc_text` on a single truncated line in `muted` (full topic on hover); channel modes move into the hover text; `{n} people` button toggling `people_hidden`; `Detach`; `Restore windows`/`Maximize` (as today); close button with hover text `Close (Ctrl+W)`. The separate maximized title row (`"#rust @ Libera"`) is removed: the sidebar shows the network.

- [ ] **Step 1: Write the failing tests** — `the_header_shows_name_topic_and_people_count_on_one_row` (name, topic text and `2 people` share a row within 8 px); `a_long_topic_is_cut_to_one_line` (500-char topic: header height equals the short-topic header height); `the_people_button_hides_and_shows_the_list` (click → no user nick painted at the right; click again → painted); `the_people_list_starts_hidden_in_a_narrow_window`.
- [ ] **Step 2:** Run them and see them fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full gate. Commit `Put the conversation name, topic and window controls on one header row`.

### Task 7: Message rows

Spec: the message list in `Main.dc.html`; stacked rows in `Narrow.dc.html`.

**Files:**
- Modify: `src/gui/mod.rs` (`draw_chat_row`, `row_layout_key`, `row_height`, `draw_scrollback`'s row loop and marker), `src/gui/formatting.rs` (`nick_color`, `render_segments` default colour), `src/gui/types.rs` (`ChatMessage`), `src/gui/logging.rs` (history rows)
- Test: `src/gui/mod.rs`, `src/gui/formatting.rs`, `src/gui/app.rs`

**Interfaces:**
- Consumes: `theme::{palette, metrics, readable_on}`, `fonts::semibold`
- Produces:
  - `pub(crate) struct RowContext<'a> { pub my_nick: &'a str, pub show_nick: bool, pub stacked: bool }`; `draw_chat_row(ui, msg, &RowContext) -> Option<MessageAction>`
  - `fn continues_previous(previous: Option<&ChatMessage>, msg: &ChatMessage) -> bool` (same sender, neither system nor action, no reply quote, same `day`)
  - `ChatMessage.day: Option<i32>` (local days since 1970-01-01, set by every constructor and by the log and archive loaders; `None` only for rows without a usable date)
  - `formatting::nick_color` is replaced by `Palette::nick`; callers pass the palette.

Rows: a three-column grid (48 | 108 | rest, gap 12, side padding 16). Time in `faint` small text, hidden when `timestamp_format == "off"` (the column collapses). Nick right-aligned in `semibold`, coloured `palette.nick(sender)` (or `text` when `color_nicks` is off; own nick always `accent`), truncated, omitted when `continues_previous`. Text in the message font family at body size, `text` colour. System rows: `muted`, small, empty nick column. Actions: `* nick text` in the text column, italic. Highlighted rows: `mention_row` fill across the row and the row's occurrence of the user's nick in `mention`; the leading `*` marker and yellow text are removed. Replies and reactions keep their behaviour inside the text column. IRC-coloured text without an IRC background goes through `readable_on(color, row_ground, 3.0)`. Below 520 px of content width rows stack: nick + time on one line (skipped when `continues_previous`), text below, side padding 14. `row_layout_key` also hashes `palette.dark`, `metrics.row_gap`, the message font, `color_nicks`, `stacked`, `show_nick` and whether timestamps are hidden. The unread marker becomes a centred `New messages` label in `accent` small semibold between two 1 px `accent` (50 % alpha) rules; `settle_read_boundary` is unchanged. A day pill (`Today`, `Yesterday`, or `YYYY-MM-DD`, small `muted` on `raise`, radius 12) is drawn above the first row of each `day`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_run_of_messages_from_one_person_shows_the_nick_once() { /* bob,bob,carol → "bob" painted once, "carol" once */ }
#[test] fn a_system_row_or_a_new_day_breaks_the_run() {}
#[test] fn nick_time_and_text_sit_in_their_columns() { /* time.left ≈ 16, nick.right ≈ 16+48+12+108, text.left ≈ +12; two rows share each edge */ }
#[test] fn a_long_nick_is_cut_inside_its_column() { /* 60-char nick: nick.right() unchanged, text.left() unchanged, row height unchanged */ }
#[test] fn hiding_timestamps_removes_the_time_column() {}
#[test] fn a_highlighted_row_is_filled_not_recoloured() { /* a mention_row rect behind the row; message text colour == palette.text */ }
#[test] fn irc_white_text_is_made_readable_on_the_light_theme() { /* "\x0300white" on light: painted colour contrast ≥ 3.0 against bg */ }
#[test] fn rows_stack_below_520_px_and_cache_their_own_heights() { /* same messages at 500 px and 900 px: different layout keys, nick above text at 500 */ }
#[test] fn changing_theme_density_or_font_remeasures_rows() { /* stats.measured > 0 on the frame after theme::apply with a different pref */ }
#[test] fn the_unread_marker_reads_new_messages() { /* update the four unread_marker tests' helper to look for "New messages" */ }
#[test] fn a_day_pill_precedes_the_first_message_of_each_day() {}
```

- [ ] **Step 2:** Run `cargo test --locked gui::tests gui::app::tests::unread_marker` and see the new ones fail.
- [ ] **Step 3:** Implement. Row height must not depend on `show_nick` in column mode (the nick shares the text's first line); it does in stacked mode, which is why both are in the key.
- [ ] **Step 4:** Run the full gate; confirm `Row measurement must not send actions or advance read state` still holds (existing tests). Commit `Lay message rows out in time, nick and text columns`.

### Task 8: People list

Spec: `<aside>` of `Main.dc.html`.

**Files:**
- Modify: `src/gui/view.rs` (`draw_user_list`, the `Panel::right` sizing in `draw_target`), `src/gui/types.rs` (`Channel`)
- Test: `src/gui/view.rs`, `src/gui/types.rs`

**Interfaces:**
- Produces: `pub(crate) enum PeopleRow<'a> { Group { label: &'static str, count: usize }, Person(&'a ChannelUser) }`; `Channel::people_rows(&self) -> Vec<PeopleRow<'_>>` (groups in order `Owners`, `Admins`, `Operators`, `Half-ops`, `Voiced`, `Members`; empty groups omitted; people keep `Channel.users` order).

Group rows use `section_label` (`{label} · {count}`); person rows use `list_row` with `dot` (filled `online`, or a `faint` ring when away), the nick (`text`, or `faint` when away) truncated, and a small `faint` note (`away`, `bot`, `away, bot`). Selection, double-click to query, hover away message and the context menu are unchanged. All rows have the same height (`control_height`) so `ScrollArea::show_rows` still virtualises. Panel: default 208, min 140, max 35 % of the window.

- [ ] **Step 1: Write the failing tests** — `people_rows_group_by_role_with_counts_and_skip_empty_groups`; `people_rows_keep_the_channel_order_inside_a_group`; `a_channel_with_5000_users_draws_only_the_visible_people` (painted nick texts < 60 at 768 px height); `away_people_are_marked_by_shape_and_note` (a painted `away` text; ring vs filled dot via distinct shape kinds).
- [ ] **Step 2:** Run them and see them fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full gate. Commit `Group the people list by role`.

### Task 9: Composer

Spec: the bottom block of `<main>` in `Main.dc.html`.

**Files:**
- Modify: `src/gui/view.rs` (`draw_input_row` and its tests)
- Test: `src/gui/view.rs`

A typing line (`{nick} is typing…`, small `muted`, fixed 20 px so the composer does not jump), then the field (`Message {target}` hint, `panel` fill, `line` stroke, radius 10, at least `control_height` tall, growing to four text rows as today), `Format` (label `Aa` with hover text `Text formatting` when stacked), `Send` filled `accent` with `on_accent` semibold text, then `Enter sends. Shift+Enter starts a new line.` in small `faint` (hidden in Compact). Behaviour, shortcuts, completion and upload handling are unchanged.

- [ ] **Step 1: Write the failing tests** — `the_composer_names_its_target_in_the_hint`; `send_is_the_accent_button_and_at_least_44_px_tall`; `the_typing_line_reserves_its_height_when_nobody_types` (composer top y identical with and without a typing nick); update `enter_sends_and_shift_enter_preserves_multiline_at_compact_sizes` and `composer_starts_on_one_row_and_bounds_long_drafts` to the new heights.
- [ ] **Step 2:** Run `cargo test --locked gui::view::` and see them fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full gate. Commit `Restyle the composer and name its target`.

### Task 10: Display settings

Spec: `Settings.dc.html`.

**Files:**
- Modify: `src/gui/dialogs.rs` (`settings_tab_display`, its caller at `:1577`, the cancel snapshot), `src/gui/app.rs` (apply on load, on change and on cancel), `src/gui/types.rs` (`timestamp_format` accepts `"off"`; `ChatMessage::configure_default_timestamp_format`)
- Test: `src/gui/dialogs.rs`, `src/gui/app.rs`

**Interfaces:**
- Consumes: `Settings::display`, `theme::apply`
- Produces: `settings_tab_display(&mut self, ui, display: &mut DisplayPrefs)`; `App::apply_display(&mut self, ctx: &egui::Context)`.

Controls in order: Theme (`Dark`, `Light`, `Match the system`), Text size (slider 12–22, shows `{n} px`), Spacing (`Comfortable`, `Compact`), Message font (`Proportional`, `Monospace`), Timestamps (`14:05`, `14:05:32`, `11-20 14:05`, `Hidden`), `Give each nickname its own color`, `Hide join, part and quit lines`, the existing scrollback and highlight-word controls, and a two-row preview drawn with `draw_chat_row`. A change applies at once; Cancel restores the opening `DisplayPrefs`; `Save & Close` persists them.

- [ ] **Step 1: Write the failing tests** — `choosing_light_applies_it_at_once` (after the click, `theme::palette(&ctx).bg == Palette::light().bg`); `cancel_restores_the_opening_display_settings`; `save_and_close_persists_theme_spacing_font_and_nick_colours`; `hidden_timestamps_round_trip_and_stop_being_drawn`; `the_display_tab_shows_a_live_preview`.
- [ ] **Step 2:** Run them and see them fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the full gate. Commit `Add theme, spacing, message font and timestamp choices to Display settings`.

### Task 11: Remaining literal colours and the light-theme sweep

**Files:**
- Modify: `src/gui/view.rs`, `src/gui/mod.rs`, `src/gui/dialogs.rs`, `src/gui/search.rs`, `src/gui/uploads.rs`, `src/gui/attention.rs`, `src/gui/automation.rs`, `src/gui/snap.rs`, `src/gui/app.rs`
- Test: `src/gui/app.rs`

Every `Color32::` literal outside `theme.rs` and the mIRC colour table in `formatting.rs` becomes a palette role: white/near-white → `text`; greys → `muted` or `faint`; green status → `online`; yellow/orange warnings → `mention`; red errors → a new `Palette.danger` (`#F28B82` dark, `#B3261E` light, added to the Task 1 contrast test); blue links → `accent`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn every_text_painted_on_the_light_theme_reads_on_light_grounds() {
    // two_networks() with a channel message, a system row, a highlighted row, an away user
    // and the Display tab open; theme::apply(Light); collect (colour, text) from Shape::Text.
    // Each colour has contrast ≥ 4.5 against at least one of bg, panel, raise, mention_row,
    // or is on_accent (drawn on accent/mention fills).
}
```
Add the same test for the dark theme.

- [ ] **Step 2:** Run it and see the light one fail with the offending texts listed.
- [ ] **Step 3:** Replace the literals; `grep -n 'Color32::' src/gui/*.rs` afterwards shows only `theme.rs`, the mIRC table, tests, and `Color32::TRANSPARENT`.
- [ ] **Step 4:** Run the full gate. Commit `Draw every colour from the theme palette`.

### Task 12: Check in the running app and update the docs

**Files:**
- Modify: `CLAUDE.md` (architecture tree: `theme.rs`, `fonts.rs`, `widgets.rs`; switchbar → sidebar; Display features), `README.md` where it describes the window
- No code tests; this task is the visual check.

- [ ] **Step 1:** Build debug; on a private display (`Xvfb :98 -screen 0 1700x900x24`, scratch `XDG_*` dirs, a fake ircd on loopback, `automatic_updates: false`) capture the main window at 1360×860 dark and light, 640×760, and the Display tab, and compare each against its artboard: panel order and widths, column edges, row heights 44/32, badge and marker colours. Fix what differs and re-run the gate.
- [ ] **Step 2:** Repeat the unread scenario from 0.1.9 (live messages, a background channel, restart) and floating/detached windows: the sidebar activates and focuses them as the switchbar did.
- [ ] **Step 3:** Cross-build `aarch64-pc-windows-gnullvm` in the `dist` profile and, only if `tasklist.exe` shows no Linefeed running, launch it once from `C:\code` to confirm Segoe UI, Segoe UI Semibold and Consolas load (log lines from `fonts::install`); remove the copy afterwards. If Linefeed is running, ask before doing anything.
- [ ] **Step 4:** Update the docs. Commit `Document the redesigned window`. Send the user the screenshots.

## Not in this plan

- Per-network collapse of sidebar groups, drag-reordering conversations, and a sidebar filter box: not in the approved artboards.
- Bundling Inter: the design's Inter is replaced by each platform's UI sans (Segoe UI on Windows) to keep the binary small and add no font licence.
- A setting to bring back the horizontal switchbar.
