//! Counting copies as they happen, instead of reading every clipboard five times a second: a
//! picture copied over a picture has no text to compare, and reading its data on each look would
//! make the app that copied it encode it again each time. Wayland: the data-control protocol's
//! selection events; X11: XFixes' selection-owner events. Our own copies count too (the caller
//! expects those).
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use wayland_client::{protocol::wl_seat::WlSeat, Display, GlobalManager};
use wayland_protocols::wlr::unstable::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
};
use x11_clipboard::xcb;
use x11_clipboard::Context;

#[derive(Default, Debug)]
pub struct Counters {
    pub regular: AtomicU64,
    pub primary: AtomicU64,
    /// whether primary selection changes are seen (Wayland: data-control version 2)
    pub primary_ok: AtomicBool,
}

impl Counters {
    pub fn get(&self, primary: bool) -> u64 {
        if primary { self.primary.load(Ordering::SeqCst) } else { self.regular.load(Ordering::SeqCst) }
    }
}

/// Selection changes on a Wayland display (its socket's name), or None if it can't be watched.
pub fn wayland(socket: &str) -> Option<Arc<Counters>> {
    let socket = socket.to_string();
    let counters = Arc::new(Counters::default());
    let mine = Arc::clone(&counters);
    let (ready, started) = mpsc::channel();
    thread::spawn(move || {
        let fail = |ready: &mpsc::Sender<bool>| { let _ = ready.send(false); };
        let Ok(display) = Display::connect_to_name(&socket) else { return fail(&ready) };
        let mut queue = display.create_event_queue();
        let attached = (*display).clone().attach(queue.token());
        let globals = GlobalManager::new(&attached);
        if queue.sync_roundtrip(&mut (), |_, _, _| {}).is_err() {
            return fail(&ready);
        }
        let (Ok(seat), Ok(manager)) = (globals.instantiate_range::<WlSeat>(1, 7),
                                       globals.instantiate_range::<ZwlrDataControlManagerV1>(1, 2)) else {
            return fail(&ready);
        };
        mine.primary_ok.store(manager.as_ref().version() >= 2, Ordering::SeqCst);
        let device = manager.get_data_device(&seat);
        device.quick_assign(move |_, event, _| {
            use zwlr_data_control_device_v1::Event;
            match event {
                Event::Selection { id } => {
                    mine.regular.fetch_add(1, Ordering::SeqCst);
                    if let Some(offer) = id { offer.destroy(); }      // (not read here: no need to keep it)
                }
                Event::PrimarySelection { id } => {
                    mine.primary.fetch_add(1, Ordering::SeqCst);
                    if let Some(offer) = id { offer.destroy(); }
                }
                _ => (),
            }
        });
        if queue.sync_roundtrip(&mut (), |_, _, _| {}).is_err() {
            return fail(&ready);
        }
        let _ = ready.send(true);
        while queue.dispatch(&mut (), |_, _, _| {}).is_ok() {}
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap_or(false).then_some(counters)
}

/// Selection changes on an X11 display (":0"), or None if it can't be watched.
pub fn x11(display: &str) -> Option<Arc<Counters>> {
    let ctx = Context::new(Some(display)).ok()?;
    let xfixes = xcb::query_extension(&ctx.connection, "XFIXES").get_reply().ok()?;
    if !xfixes.present() {
        return None;
    }
    xcb::xfixes::query_version(&ctx.connection, 5, 0).get_reply().ok()?;
    let root = ctx.connection.get_setup().roots().nth(ctx.screen as usize)?.root();
    let mask = xcb::xfixes::SELECTION_EVENT_MASK_SET_SELECTION_OWNER
        | xcb::xfixes::SELECTION_EVENT_MASK_SELECTION_CLIENT_CLOSE
        | xcb::xfixes::SELECTION_EVENT_MASK_SELECTION_WINDOW_DESTROY;
    for selection in [ctx.atoms.clipboard, ctx.atoms.primary] {
        xcb::xfixes::select_selection_input(&ctx.connection, root, selection, mask);
    }
    ctx.connection.flush();
    let counters = Arc::new(Counters::default());
    counters.primary_ok.store(true, Ordering::SeqCst);
    let mine = Arc::clone(&counters);
    let (base, clipboard) = (xfixes.first_event(), ctx.atoms.clipboard);
    thread::spawn(move || {
        while let Some(event) = ctx.connection.wait_for_event() {
            if event.response_type() & !0x80 == base + xcb::xfixes::SELECTION_NOTIFY {
                let ev: &xcb::xfixes::SelectionNotifyEvent = unsafe { xcb::cast_event(&event) };
                if ev.selection() == clipboard {
                    mine.regular.fetch_add(1, Ordering::SeqCst);
                } else {
                    mine.primary.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    });
    Some(counters)
}
