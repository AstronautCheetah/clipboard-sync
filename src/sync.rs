use chrono::Local;
use std::env;
use std::{thread::sleep, time::Duration};
use wayland_client::ConnectError;
use wl_clipboard_rs::paste::Error as PasteError;

use crate::clipboard::*;
use crate::watch;
use crate::error::{MyError, MyResult, StandardizedError};
use crate::log::{self, concise_numbers};

pub fn get_clipboards() -> MyResult<Vec<Box<dyn Clipboard>>> {
    // Our own desktop's Xwayland ($DISPLAY beside $WAYLAND_DISPLAY; read before the Wayland
    // clipboards are probed, which changes WAYLAND_DISPLAY) is left out: the compositor already
    // passes copies between it and the Wayland clipboard (KWin: X to Wayland at once, Wayland to
    // X while an X window has focus). Synced as well, the text written to it went on to the
    // Wayland side and replaced what had just been copied there (a file cut in Dolphin then
    // pasted only as its address).
    let own_xwayland = env::var("WAYLAND_DISPLAY")
        .ok()
        .and(env::var("DISPLAY").ok())
        .map(|d| d.trim_end_matches(".0").to_string());
    log::debug!("identifying clipboards...");
    let mut clipboards = get_clipboards_spec(get_wayland);
    clipboards.extend(get_clipboards_spec(get_x11).into_iter().filter(|c| {
        let ours = own_xwayland.as_deref() == Some(c.display().as_str());
        if ours {
            log::debug!("leaving out {c:?}: this desktop's own Xwayland");
        }
        !ours
    }));
    // Each display's primary selection (select, then middle-click) too, kept in step with the
    // others' primary selections, apart from the clipboards.
    let twins: Vec<Box<dyn Clipboard>> = clipboards.iter().filter_map(|c| c.primary_twin()).collect();
    clipboards.extend(twins);
    // Nothing is written at the start (this runs again every 10 minutes, when the worker is
    // restarted): no test values to find clipboards that are one (the only pair on the Frame is
    // left out above), and no text shared until something new is copied. Both replaced what
    // every clipboard held, a file or an image not pasted yet included.
    log::info!("Using clipboards: {:?}", clipboards);

    Ok(clipboards)
}

// A password shared from this display (its password manager marked it secret): when it's emptied
// there, the copies go too. Kept in a file of the runtime folder (memory; gone at logout) across
// the worker's restarts: the display only, never the password.
fn secret_note() -> Option<std::path::PathBuf> {
    env::var("XDG_RUNTIME_DIR").ok().map(|d| std::path::Path::new(&d).join("clipboard-sync-secret"))
}

fn note_secret(display: Option<&str>) {
    if let Some(path) = secret_note() {
        let _ = match display {
            Some(d) => std::fs::write(&path, d),
            None => std::fs::remove_file(&path).or(Ok::<(), std::io::Error>(())),
        };
    }
}

fn clear_copies(clipboards: &Vec<Box<dyn Clipboard>>, st: &mut State, source: usize) -> MyResult<()> {
    log::info!("secret emptied on display {}: emptying its copies", clipboards[source].display());
    for j in 0..clipboards.len() {
        if j != source && !clipboards[j].primary() && !st.known[j].is_empty() {
            st.put(clipboards, j, &Content::default())?;
        }
    }
    Ok(())
}

/// What the loop knows of each clipboard: what it holds (as seen, or as put there), its change
/// count when last looked at, and how many of the changes still to come are our own puts.
struct State {
    known: Vec<Content>,
    counted: Vec<Option<u64>>,
    ours: Vec<u64>,
}

impl State {
    fn put(&mut self, clipboards: &Vec<Box<dyn Clipboard>>, j: usize, content: &Content) -> MyResult<()> {
        let c = &clipboards[j];
        c.put(content)?;
        self.known[j] = content.clone();
        if c.changes().is_some() {
            self.ours[j] += 1;
        }
        Ok(())
    }

    /// Whether clipboard i holds something new: a change counted that isn't one of our own puts
    /// (one of those whose text differs counts all the same: a copy made meanwhile); a clipboard
    /// that isn't watched: its text.
    fn changed(&mut self, c: &dyn Clipboard, i: usize) -> MyResult<bool> {
        match (c.changes(), self.counted[i]) {
            (Some(now), Some(before)) => {
                if now == before {
                    return Ok(false);
                }
                self.counted[i] = Some(now);
                let new = now - before;
                if self.ours[i] >= new {
                    self.ours[i] -= new;
                    return Ok(c.get()? != self.known[i].text);
                }
                self.ours[i] = 0;
                Ok(true)
            }
            (now, _) => {
                self.counted[i] = now;
                Ok(c.get()? != self.known[i].text)
            }
        }
    }
}

pub fn keep_synced(clipboards: &Vec<Box<dyn Clipboard>>) -> MyResult<()> {
    if clipboards.is_empty() {
        return Err(MyError::NoClipboards);
    }
    let n = clipboards.len();
    // A copy goes to the other clipboards of its kind (the clipboards, or the primary
    // selections), never back where it came from: that one holds it as the app made it (a cut,
    // formats beyond those shared, a password manager's mark), which our copy would replace.
    let mut st = State {
        known: clipboards.iter().map(|c| c.content().unwrap_or_default()).collect(),
        counted: clipboards.iter().map(|c| c.changes()).collect(),
        ours: vec![0; n],
    };
    // One with nothing at all on it gets what the others hold: a copy the worker before this
    // one shared there is gone (X11 drops a selection with its owner), and a restart mustn't
    // lose it. One with anything on it is left alone.
    for primary in [false, true] {
        let same = |j: usize| clipboards[j].primary() == primary;
        let Some(fill) = (0..n).filter(|&j| same(j)).map(|j| st.known[j].clone()).find(|k| !k.is_empty()) else {
            continue;
        };
        for j in (0..n).filter(|&j| same(j)) {
            if st.known[j].is_empty() && clipboards[j].is_empty() {
                log::debug!("filling empty {:?} ({})", clipboards[j], fill.describe());
                st.put(clipboards, j, &fill)?;
            }
        }
    }
    let mut secret_from: Option<usize> = None;
    // (a password shared before the worker restarted: still there, or emptied meanwhile)
    let noted = secret_note().and_then(|p| std::fs::read_to_string(p).ok());
    if let Some(i) = noted.and_then(|d| clipboards.iter().position(|c| !c.primary() && c.display() == d)) {
        if st.known[i].is_empty() {
            clear_copies(clipboards, &mut st, i)?;
            note_secret(None);
        } else if (0..n).filter(|&j| !clipboards[j].primary()).all(|j| st.known[j].text == st.known[i].text) {
            secret_from = Some(i);
        } else {
            note_secret(None);
        }
    }
    loop {
        sleep(Duration::from_millis(200));
        for i in 0..n {
            let c = &clipboards[i];
            if !c.should_poll() || !st.changed(&**c, i)? {
                continue;
            }
            let now = c.content()?;
            if now == st.known[i] {
                continue;
            }
            st.known[i] = now.clone();
            if now.is_empty() {
                // nothing shared on it any more (emptied, or something we don't share): only a
                // shared password follows, gone from the copies too
                if secret_from == Some(i) {
                    clear_copies(clipboards, &mut st, i)?;
                    secret_from = None;
                    note_secret(None);
                }
                continue;
            }
            log::info!("{} updated on display {}: {}", if c.primary() { "primary selection" } else { "clipboard" },
                       c.display(), now.describe());
            log::sensitive!(log::info, "clipboard contents: '{}'", now.text);
            for j in 0..n {
                if j != i && clipboards[j].primary() == c.primary() && st.known[j] != now {
                    st.put(clipboards, j, &now)?;
                }
            }
            if !c.primary() {
                secret_from = if now.secret { Some(i) } else { None };
                note_secret(secret_from.map(|_| c.display()).as_deref());
            }
        }
    }
}

// /// equality comparison is the bottleneck, and it's composed of two steps. so
// /// the purpose of this function is to minimize the number of executions of
// /// those steps. to do so, these steps are run at different times and combined
// /// at the end. this requires some extra computation in here but that's fine.
// fn dedupe(clipboards: Vec<Box<dyn Clipboard>>) -> MyResult<Vec<Box<dyn Clipboard>>> {
//     let mut k_cant_read_v: HashMap<usize, HashSet<usize>> = HashMap::new();
//     let mut k_can_read_v: HashMap<usize, HashSet<usize>> = HashMap::new();
//     for (i, one) in clipboards.iter().enumerate() {
//         // let i_can_read = k_can_read_v.entry(i).or_insert(HashSet::new()).clone();
//         let i_cant_read = k_cant_read_v.entry(i).or_insert(HashSet::new()).clone();
//         let d1 = &one.display();
//         one.set(d1)?;
//         for (j, two) in clipboards.iter().enumerate() {
//             println!("{i} {j} {} {}", i != j, !i_cant_read.contains(&j));
//             if i != j && !i_cant_read.contains(&j) {
//                 if d1 == &two.get()? {
//                     log::debug!("{two:?} can read {one:?}");
//                     k_can_read_v.entry(j).or_insert(HashSet::new()).insert(i);
//                 } else {
//                     log::debug!("{two:?} cannot read {one:?}");
//                     k_cant_read_v.entry(j).or_insert(HashSet::new()).insert(i);
//                 }
//             }
//         }
//     }
//     let mut remove_me: HashSet<usize> = HashSet::new();
//     let mut dont_remove_me: HashSet<usize> = HashSet::new();
//     println!("{k_cant_read_v:#?}");
//     println!("{k_can_read_v:#?}");
//     for (k, v) in k_can_read_v.iter() {
//         for readable in v {
//             if !dont_remove_me.contains(readable)
//                 && k_can_read_v
//                     .get(readable)
//                     .map(|what_readable_can_read| what_readable_can_read.contains(k))
//                     .unwrap_or(false)
//             {
//                 remove_me.insert(*readable);
//                 dont_remove_me.insert(*k);
//             }
//         }
//     }

//     Ok(clipboards
//         .into_iter()
//         .enumerate()
//         .filter(|(i, _)| !remove_me.contains(i))
//         .map(|(_, c)| c)
//         .collect::<Vec<Box<dyn Clipboard>>>())
// }

// /// equality comparison is the bottleneck, and it's composed of two steps. so
// /// the purpose of this function is to minimize the number of executions of
// /// those steps. to do so, these steps are run at different times and combined
// /// at the end. this requires some extra computation in here but that's fine.
// fn dedupe(clipboards: Vec<Box<dyn Clipboard>>) -> MyResult<Vec<Box<dyn Clipboard>>> {
//     for (i, clipboard) in clipboards.iter().enumerate() {
//         println!("write {i}");
//         clipboard.set(&format!("{}{i}", clipboard.get()?))?;
//     }
//     let mut results = HashMap::new();
//     for (i, clipboard) in clipboards.iter().enumerate() {
//         println!("read {i}");
//         results.insert(i, clipboard.get()?);
//     }

//     todo!()
// }

fn get_clipboards_spec<F: Fn(u8) -> MyResult<Option<Box<dyn Clipboard>>>>(
    getter: F,
) -> Vec<Box<dyn Clipboard>> {
    let mut clipboards: Vec<Box<dyn Clipboard>> = Vec::new();
    let mut xcb_conn_err = None;
    let mut xcb_conn_failed_clipboards = vec![];
    for i in 0..u8::MAX {
        let result = getter(i);
        match result {
            Ok(option) => {
                if let Some(clipboard) = option {
                    log::debug!("Found clipboard: {:?}", clipboard);
                    clipboards.push(clipboard);
                }
            }
            Err(MyError::TerminalClipboard(StandardizedError {
                inner,
                stdio: None,
            })) if format!("{inner}") == "clipboard error: X11 clipboard error : XCB connection error: Connection" => {
                xcb_conn_failed_clipboards.push(i);
                xcb_conn_err = Some(inner);
            },
            Err(err) => log::error!(
                "unexpected error while attempting to setup clipboard {}: {}",
                i,
                err
            ),
        }
    }
    if let Some(err) = xcb_conn_err {
        let displays = concise_numbers(&xcb_conn_failed_clipboards);
        log::warning!(
            "Issue connecting to some x11 clipboards. \
This is expected when hooking up to gnome wayland, and not a problem in that context. \
Details: '{err}' for x11 displays: {displays}",
        );
    }

    clipboards
}

fn get_wayland(n: u8) -> MyResult<Option<Box<dyn Clipboard>>> {
    let wl_display = format!("wayland-{}", n);
    let clipboard = WlrClipboard::new(wl_display.clone(), false, None);
    let attempt = clipboard.get();
    if let Err(MyError::WlcrsPaste(PasteError::WaylandConnection(
        ConnectError::NoCompositorListening,
    ))) = attempt
    {
        return Ok(None);
    }
    if let Err(MyError::WlcrsPaste(PasteError::MissingProtocol {
        name: "zwlr_data_control_manager_v1",
        version: 1,
    })) = attempt
    {
        log::warning!(
            "{wl_display} does not support zwlr_data_control_manager_v1. If you are running \
gnome in wayland, that's OK because it provides an x11 clipboard, which will be used instead. \
Otherwise, `wl-copy` will be used to sync data *into* this clipboard, but it will not be possible \
to read data *from* this clipboard into other clipboards."
        );
        let command = WlCommandClipboard {
            display: wl_display.clone(),
        };
        let Ok(gotten) = command.get() else {
            return Ok(None);
        };
        let Ok(_) = command.set(&gotten) else {
            return Ok(None);
        };
        return Ok(Some(Box::new(command)));
    }
    attempt?;
    let watch = watch::wayland(&wl_display);
    if watch.is_none() {
        log::warning!("{wl_display}: its copies can't be watched, so only text is compared (as before)");
    }

    Ok(Some(Box::new(WlrClipboard::new(wl_display, false, watch))))
}

fn get_x11(n: u8) -> MyResult<Option<Box<dyn Clipboard>>> {
    let display = format!(":{}", n);
    let clipboard = X11Clipboard::new(display.clone(), false, None)?;
    clipboard.get()?;

    Ok(Some(Box::new(clipboard.watched(watch::x11(&display)))))
}
