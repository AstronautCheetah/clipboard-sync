//! An X11 selection owner that offers several formats at once (x11-clipboard's offers one): text
//! as UTF8_STRING and its kin, a file list as text/uri-list beside it, an image as image/png, a
//! password manager's mark. CLIPBOARD or PRIMARY, with large data sent in pieces (INCR).
//! Adapted from x11-clipboard 0.5.3's run.rs (MIT, quininer).
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use x11_clipboard::xcb::{self, Atom, Window};
use x11_clipboard::Context;

type Formats = Arc<Vec<(Atom, Arc<Vec<u8>>)>>;

pub struct Owner {
    ctx: Arc<Context>,
    data: Arc<Mutex<HashMap<Atom, Formats>>>,
}

impl Owner {
    pub fn new(display: &str) -> Option<Self> {
        let ctx = Arc::new(Context::new(Some(display)).ok()?);
        let data = Arc::new(Mutex::new(HashMap::new()));
        let (c, d) = (Arc::clone(&ctx), Arc::clone(&data));
        thread::spawn(move || serve(&c, &d));
        Some(Self { ctx, data })
    }

    pub fn atom(&self, name: &str) -> Atom {
        self.ctx.get_atom(name).unwrap_or(xcb::ATOM_NONE)
    }

    /// Own `selection` with these formats (format atom, data); none: give it up.
    pub fn own(&self, selection: Atom, formats: Vec<(Atom, Vec<u8>)>) -> bool {
        let empty = formats.is_empty();
        if let Ok(mut d) = self.data.lock() {
            d.insert(selection, Arc::new(formats.into_iter().map(|(a, v)| (a, Arc::new(v))).collect()));
        }
        let owner = if empty { xcb::NONE } else { self.ctx.window };
        xcb::set_selection_owner(&self.ctx.connection, owner, selection, xcb::CURRENT_TIME);
        self.ctx.connection.flush();
        empty
            || xcb::get_selection_owner(&self.ctx.connection, selection)
                .get_reply()
                .map(|r| r.owner() == self.ctx.window)
                .unwrap_or(false)
    }
}

struct Incr {
    target: Atom,
    value: Arc<Vec<u8>>,
    pos: usize,
}

fn serve(ctx: &Context, data: &Mutex<HashMap<Atom, Formats>>) {
    let max = ctx.connection.get_maximum_request_length() as usize * 4;
    let chunk = std::cmp::min(max.saturating_sub(64), 256 * 1024).max(x11_clipboard::INCR_CHUNK_SIZE);
    let mut incr: HashMap<(Window, Atom), Incr> = HashMap::new();
    while let Some(event) = ctx.connection.wait_for_event() {
        match event.response_type() & !0x80 {
            xcb::SELECTION_REQUEST => {
                let ev: &xcb::SelectionRequestEvent = unsafe { xcb::cast_event(&event) };
                let formats = data.lock().ok().and_then(|d| d.get(&ev.selection()).cloned());
                // (an obsolete requestor names no property: the target is used)
                let property = if ev.property() == xcb::NONE { ev.target() } else { ev.property() };
                let mut ok = false;
                if let Some(formats) = formats.filter(|f| !f.is_empty()) {
                    if ev.target() == ctx.atoms.targets {
                        let mut list: Vec<Atom> = vec![ctx.atoms.targets];
                        list.extend(formats.iter().map(|(a, _)| *a));
                        xcb::change_property(&ctx.connection, xcb::PROP_MODE_REPLACE as u8, ev.requestor(), property,
                                             xcb::ATOM_ATOM, 32, &list);
                        ok = true;
                    } else if let Some((target, value)) = formats.iter().find(|(a, _)| *a == ev.target()) {
                        if value.len() < chunk {
                            xcb::change_property(&ctx.connection, xcb::PROP_MODE_REPLACE as u8, ev.requestor(),
                                                 property, *target, 8, &value[..]);
                        } else {
                            // in pieces: the requestor deletes the property for each next one
                            xcb::change_window_attributes(&ctx.connection, ev.requestor(),
                                                          &[(xcb::CW_EVENT_MASK, xcb::EVENT_MASK_PROPERTY_CHANGE)]);
                            xcb::change_property(&ctx.connection, xcb::PROP_MODE_REPLACE as u8, ev.requestor(),
                                                 property, ctx.atoms.incr, 32, &[value.len() as u32]);
                            incr.insert((ev.requestor(), property),
                                        Incr { target: *target, value: Arc::clone(value), pos: 0 });
                        }
                        ok = true;
                    }
                }
                let notify = xcb::SelectionNotifyEvent::new(ev.time(), ev.requestor(), ev.selection(), ev.target(),
                                                            if ok { property } else { xcb::NONE });
                xcb::send_event(&ctx.connection, false, ev.requestor(), 0, &notify);
                ctx.connection.flush();
            }
            xcb::PROPERTY_NOTIFY => {
                let ev: &xcb::PropertyNotifyEvent = unsafe { xcb::cast_event(&event) };
                if ev.state() != xcb::PROPERTY_DELETE as u8 {
                    continue;
                }
                let key = (ev.window(), ev.atom());
                let done = match incr.get_mut(&key) {
                    None => continue,
                    Some(st) => {
                        let len = std::cmp::min(chunk, st.value.len() - st.pos);
                        xcb::change_property(&ctx.connection, xcb::PROP_MODE_REPLACE as u8, ev.window(), ev.atom(),
                                             st.target, 8, &st.value[st.pos..st.pos + len]);
                        st.pos += len;
                        len == 0
                    }
                };
                if done {
                    incr.remove(&key);
                }
                ctx.connection.flush();
            }
            xcb::SELECTION_CLEAR => {
                let ev: &xcb::SelectionClearEvent = unsafe { xcb::cast_event(&event) };
                if let Ok(mut d) = data.lock() {
                    d.remove(&ev.selection());
                }
            }
            _ => (),
        }
    }
}

/// The formats `selection` is offered in (asked with ctx's window), so only those are asked
/// for: Some([]) when nothing owns it, None when no answer came within half a second.
/// (x11-clipboard's load can't ask: the answer, a list of type ATOM, isn't the type it asked for.)
pub fn targets(ctx: &Context, selection: Atom) -> Option<Vec<String>> {
    use std::time::{Duration, Instant};
    let property = ctx.get_atom("FRAME_TWEAKS_TARGETS").ok()?;
    xcb::convert_selection(&ctx.connection, ctx.window, selection, ctx.atoms.targets, property, xcb::CURRENT_TIME);
    ctx.connection.flush();
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match ctx.connection.poll_for_event() {
            Some(event) if event.response_type() & !0x80 == xcb::SELECTION_NOTIFY => {
                let ev: &xcb::SelectionNotifyEvent = unsafe { xcb::cast_event(&event) };
                if ev.selection() != selection {
                    continue;
                }
                if ev.property() == xcb::NONE {
                    return Some(vec![]);
                }
                let reply = xcb::get_property(&ctx.connection, true, ctx.window, property, xcb::ATOM_ATOM, 0, 4096)
                    .get_reply()
                    .ok()?;
                let atoms: &[Atom] = reply.value();
                return Some(atoms.iter()
                    .filter_map(|a| xcb::get_atom_name(&ctx.connection, *a).get_reply().ok().map(|r| r.name().to_string()))
                    .collect());
            }
            Some(_) => continue,
            None if Instant::now() > deadline => return None,
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
}
