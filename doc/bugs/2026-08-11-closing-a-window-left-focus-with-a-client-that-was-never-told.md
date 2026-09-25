# Closing a window left focus with a client that was never told

## Status

Fixed in `01048a6a`. Every `WindowRegistry` call that moves focus returns the
window that has to be told (`create_window`, `set_minimized`,
`release_dock_focus`, `destroy_window`, `destroy_windows_for_pid`), and the
callers send `focus_gained` after dropping the registry lock.

## Symptoms

After quitting a GUI program, every keystroke was dropped. The terminal's title
bar and task button both painted focused, and clicking the terminal did not
help. Recovery took a reboot.

## Root cause

Three pieces each behaved correctly, and their composition lost the keyboard.

`WindowRegistry::destroy_window` moved focus to `topmost_focusable` and stored
it, so `/proc/windows`, the compositor's decorations and the panel all named
the terminal focused. It did not send `FocusGained`. A client's belief about
focus comes only from focus events, and the `edos_render` terminal widget drops
`on_key` when it believes it is unfocused. The terminal had been sent
`FocusLost` when the other window was created, and nothing told it otherwise.

Click-to-focus could not repair it. `handle_mouse_event` sends nothing when the
click target already equals `registry.focused_window()`, which is right (a
click inside the focused window must not restage focus) but means the registry
and the client can never resynchronise once they disagree.

Both destroy paths needed the fix, and they are different code:
`sys_window_destroy` for a program closing its own window (`Drop for Window`
in `edos_render`), and `window::cleanup_process_windows` for a killed or
panicking one, which sends the event after removing the dead event queues.

## Reasoning rules going forward

- A focus transition is real only when its event is delivered. Any registry
  change to focus returns the window to notify, and the caller notifies it
  outside the lock.
- A window that renders focused is not a window that receives keys:
  decorations read the registry's `focused` flag, while the client acts on its
  last focus event.

## If this reappears

1. The symptom is a window that looks live and behaves dead.
2. `/proc/windows` shows the registry's answer; the client's answer is its
   last focus event, which no program logs by default. Log the client's
   `FocusGained`/`FocusLost` handling with `edos_lib::io::klog_dump` and read
   `run_log.txt`. No `FocusGained` after a close is this bug.
3. To test the kill path, the program must hold focus when it dies. Use a
   delayed kill, `sh -c "sleep 8; kill <pid>" &`, then click the target window.
   Get the pid without reading the covered terminal by running
   `ps > /dev/klog` and `scripts/edos-vm log` on the host.
