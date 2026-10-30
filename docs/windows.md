# Servers and windows

Linefeed can be on several IRC networks at once. Each connection has a
**status window** (server messages, MOTD, numerics) plus one window per
channel and private-message query, in the style of mIRC.

## Connecting to several networks

- **Window → New connection...** opens a fresh status window with the Connect
  dialog. Pick a saved favorite or fill in a server and click Connect.
- `/server -m <host> [port]` connects to another server in a new status
  window. Like `/server`, it never reuses the current network's password,
  SASL credentials or auto-perform.
- `/server <host> [port]` still switches the current connection.
- Each favorite in the Connect dialog has a checkbox: checked favorites
  connect automatically when Linefeed starts. Without any, Linefeed starts
  with one status window and the Connect dialog, as before.
- Linefeed refuses a second connection to the same server, port and TLS
  setting as a live one and brings the existing status window forward:
  both would share log files and nick state.

## Window layout

The switchbar under the toolbar lists every window, grouped by network:
unread channels in green, unread queries in orange, status windows with
new server messages in yellow. Click a window to make it active.

- **Maximized** (the default): only the active window is shown and it
  fills the main window, as in earlier versions.
- **Floating**: every window is a movable, resizable child window.
  **Window → Tile** lays them out side by side, **Window → Cascade**
  stacks them. Windows that open later (a new query, a channel you join)
  open in a cascade slot behind the window you are typing in.

`Ctrl+M` (or **Window → Maximize/Restore windows**) switches between the two.

### Snap layouts

As in Windows 11, floating windows snap into layouts inside the chat area:

- Rest the pointer on a window's maximize button (□), or on **Restore**
  when windows are maximized, and a grid of layouts opens: halves, two
  thirds and a third, thirds, a half and two quarters, and quarters. Click
  a zone of a layout to put the window there.
- Drag a window by its caption to the top of the chat area and the same
  grid appears; drop the window on a zone. Drag it to the left or right
  edge for that half, or into a corner for that quarter.
- After a window snaps, each zone left over lists the other windows
  (Snap Assist): click one to fill the zone. `Escape` or a click elsewhere
  dismisses it.

In a chat area taller than it is wide, the layouts are turned on their
side. Clicking the maximize button, or double-clicking a caption,
maximizes that window.
`Ctrl+Tab` / `Ctrl+Shift+Tab` and `Alt+1`..`Alt+9` move between windows in
switchbar order across all networks.

## Detached windows

Right-click a window on the switchbar (or use **Detach** on a maximized
window, or `/window detach`) to move it into its own OS window, for example
onto another monitor. The detached window has its own topic, user list and
input line; **Reattach** (or `/window attach`) puts it back.

Detached windows stay on screen and keep working while the main window is
minimized or covered, for example with the main window minimized and a
channel on a second monitor. When Linefeed goes to the system tray
(Windows), they hide with it, and restoring it from the tray or by starting
it again brings them back where they were. Connections keep running
throughout.

Dialogs open inside the detached window they were asked for from:
channel info, the channel list, the colour picker, and also Settings,
Connect and the question before disconnecting a server. So they are seen
even while the main window is minimized.

In floating mode, child windows keep their place relative to the chat area:
when the main window is resized, or the switchbar wraps onto another line,
they move and resize with it instead of ending up under the switchbar or
off-screen.

## Closing windows

Closing a window works like mIRC, for child windows and detached windows
alike (`Ctrl+W` closes the active one):

- a **channel** window parts the channel; it closes when the server
  confirms,
- a **query** window closes,
- a **status** window asks before disconnecting that server and closing
  all of its windows. Closing the last one disconnects and shows the
  Connect dialog.

## What is remembered

Window placement (maximized or floating, each window's position and size,
which windows were detached and where) is saved in `layout.json` next to
`settings.json`, keyed by network and channel name, so a channel you rejoin
later or a network that reconnects finds its old place. It is kept apart from
`settings.json` so that moving a window never rewrites the file that holds
credentials. Restarting to install an update reconnects every network that
was connected (see `auto-updates.md`).

## Commands

| Command | Action |
|---------|--------|
| `/window tile` / `/window cascade` | Arrange the windows |
| `/window max` / `/window restore` | One window at a time / all floating |
| `/window detach` / `/window attach` | Move this window to its own OS window and back |
| `/window close` | Close this window (parts a channel) |
| `/window new` | Open a status window for another server |
| `/server -m <host> [port]` | Connect to another server in a new status window |
