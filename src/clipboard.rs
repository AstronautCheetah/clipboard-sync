use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::{env, io::Read, process::Command, time::Duration};
use terminal_clipboard::Clipboard as TerminalClipboard;
use wl_clipboard_rs::copy::{self as wl_copy, MimeSource, MimeType as CopyMimeType, Options, Source};
use wl_clipboard_rs::paste::{
    get_contents, get_mime_types, ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat,
};

use crate::error::{Generify, MyResult, Standardize};
use crate::owner::{self, Owner};
use crate::watch::Counters;

/// What password managers (KeePassXC, …) add to a password they copy, set to "secret", so
/// clipboard histories leave it out (KDE's convention).
const SECRET_HINT: &str = "x-kde-passwordManagerHint";
/// The picture formats shared, the first one offered in this order, and the largest shared.
const IMAGES: [&str; 5] = ["image/png", "image/jpeg", "image/bmp", "image/gif", "image/webp"];
const MAX_IMAGE: usize = 32 << 20;

/// What a clipboard holds, as it's shared: its text, a file list (text/uri-list of file:// URLs:
/// both sessions see the same files), a picture (its format and data), and whether a password
/// manager marked it secret. Formats beyond these stay where they were copied.
#[derive(Clone, Default, PartialEq)]
pub struct Content {
    pub text: String,
    pub files: Vec<u8>,
    pub image: Option<(String, Vec<u8>)>,
    pub secret: bool,
}

impl Content {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.files.is_empty() && self.image.is_none()
    }

    /// (for the log: what, not the contents)
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        if !self.text.is_empty() {
            parts.push(format!("text ({} bytes)", self.text.len()));
        }
        if !self.files.is_empty() {
            parts.push(format!("{} file(s)", self.uris().len()));
        }
        if let Some((mime, data)) = &self.image {
            parts.push(format!("{mime} ({} bytes)", data.len()));
        }
        if self.secret {
            parts.push("marked secret".into());
        }
        if parts.is_empty() { "nothing".into() } else { parts.join(", ") }
    }

    fn uris(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.files)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    }

    /// GTK's form of a file list: "copy", then one URL a line. (A cut is shared as a copy, so a
    /// file never moves because of a paste in the other session.)
    fn gnome_files(&self) -> Vec<u8> {
        format!("copy\n{}", self.uris().join("\n")).into_bytes()
    }
}

/// A file list worth sharing: text/uri-list whose entries are all local files; else nothing.
fn file_list(data: Vec<u8>) -> Vec<u8> {
    let c = Content { files: data, ..Default::default() };
    let uris = c.uris();
    if !uris.is_empty() && uris.iter().all(|u| u.starts_with("file://")) { c.files } else { vec![] }
}

pub trait Clipboard: std::fmt::Debug {
    fn display(&self) -> String;

    fn get(&self) -> MyResult<String>;

    fn set(&self, value: &str) -> MyResult<()>;

    fn should_poll(&self) -> bool {
        true
    }

    fn rank(&self) -> u8 {
        100
    }

    /// Whether what it holds now is marked secret (SECRET_HINT).
    fn is_secret(&self) -> bool {
        false
    }

    /// Set it, marked secret where the clipboard can carry the mark.
    fn set_secret(&self, value: &str) -> MyResult<()> {
        self.set(value)
    }

    /// Empty it.
    fn clear(&self) -> MyResult<()> {
        self.set("")
    }

    /// Whether nothing at all is on it (not even an image or a file); unsure: no.
    fn is_empty(&self) -> bool {
        false
    }

    /// The primary selection (select, then middle-click), not the clipboard.
    fn primary(&self) -> bool {
        false
    }

    /// How many times what it holds has changed (copies, ours too), if that's watched.
    fn changes(&self) -> Option<u64> {
        None
    }

    /// All of what it holds that's shared (only the text, on a clipboard that can't do more).
    fn content(&self) -> MyResult<Content> {
        Ok(Content { text: self.get()?, secret: self.is_secret(), ..Default::default() })
    }

    /// Make it hold this; an empty content empties it.
    fn put(&self, c: &Content) -> MyResult<()> {
        if c.is_empty() {
            self.clear()
        } else if c.secret {
            self.set_secret(&c.text)
        } else {
            self.set(&c.text)
        }
    }

    /// The same display's primary selection, as a clipboard of its own, where there is one.
    fn primary_twin(&self) -> Option<Box<dyn Clipboard>> {
        None
    }
}

impl<T: Clipboard> Clipboard for Box<T> {
    fn get(&self) -> MyResult<String> {
        (**self).get()
    }

    fn set(&self, value: &str) -> MyResult<()> {
        (**self).set(value)
    }

    fn display(&self) -> String {
        (**self).display()
    }

    fn should_poll(&self) -> bool {
        (**self).should_poll()
    }

    fn rank(&self) -> u8 {
        (**self).rank()
    }

    fn is_secret(&self) -> bool {
        (**self).is_secret()
    }

    fn set_secret(&self, value: &str) -> MyResult<()> {
        (**self).set_secret(value)
    }

    fn clear(&self) -> MyResult<()> {
        (**self).clear()
    }

    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn primary(&self) -> bool {
        (**self).primary()
    }

    fn changes(&self) -> Option<u64> {
        (**self).changes()
    }

    fn content(&self) -> MyResult<Content> {
        (**self).content()
    }

    fn put(&self, c: &Content) -> MyResult<()> {
        (**self).put(c)
    }

    fn primary_twin(&self) -> Option<Box<dyn Clipboard>> {
        (**self).primary_twin()
    }
}

#[derive(Debug)]
pub struct WlrClipboard {
    pub display: String,
    pub primary: bool,
    pub watch: Option<Arc<Counters>>,
}

impl WlrClipboard {
    pub fn new(display: String, primary: bool, watch: Option<Arc<Counters>>) -> Self {
        Self { display, primary, watch }
    }

    fn paste_type(&self) -> ClipboardType {
        if self.primary { ClipboardType::Primary } else { ClipboardType::Regular }
    }

    fn copy_type(&self) -> wl_copy::ClipboardType {
        if self.primary { wl_copy::ClipboardType::Primary } else { wl_copy::ClipboardType::Regular }
    }

    /// One format's data, up to max bytes (larger: none).
    fn read(&self, mime: &str, max: usize) -> Option<Vec<u8>> {
        let (pipe, _) = get_contents(self.paste_type(), Seat::Unspecified, PasteMimeType::Specific(mime)).ok()?;
        let mut data = vec![];
        pipe.take(max as u64 + 1).read_to_end(&mut data).ok()?;
        (data.len() <= max).then_some(data)
    }
}

impl WlrClipboard {
    /// Offer these formats (data, MIME type) on it; MimeType::Text offers the usual text types.
    fn offer(&self, formats: Vec<(Vec<u8>, CopyMimeType)>) -> MyResult<()> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let sources: Vec<MimeSource> = formats
            .into_iter()
            .map(|(data, mime_type)| MimeSource { source: Source::Bytes(data.into()), mime_type })
            .collect();
        let mut opts = Options::new();
        opts.clipboard(self.copy_type());
        let result = std::panic::catch_unwind(|| opts.copy_multi(sources));

        Ok(result.standardize().generify()??)
    }
}

impl Clipboard for WlrClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        // Plain text only. wl-clipboard-rs counts any text/* as text, so an image copied with an
        // HTML <img> beside it (for apps that want one) would spread as markup and replace the image.
        if let Ok(types) = get_mime_types(self.paste_type(), Seat::Unspecified) {
            let plain = |t: &String| t.starts_with("text/plain") || ["UTF8_STRING", "STRING", "TEXT"].contains(&t.as_str());
            if !types.iter().any(plain) {
                return Ok("".to_string());
            }
        }
        let result = get_contents(
            self.paste_type(),
            Seat::Unspecified,
            PasteMimeType::Text,
        );

        match result {
            Ok((mut pipe, _)) => {
                let mut contents = vec![];
                pipe.read_to_end(&mut contents)?;
                Ok(String::from_utf8_lossy(&contents).to_string())
            }

            Err(PasteError::NoSeats)
            | Err(PasteError::ClipboardEmpty)
            | Err(PasteError::NoMimeType) => Ok("".to_string()),

            Err(err) => Err(err)?,
        }
    }

    fn set(&self, value: &str) -> MyResult<()> {
        self.offer(vec![(value.as_bytes().to_vec(), CopyMimeType::Text)])
    }

    fn rank(&self) -> u8 {
        10
    }

    fn is_secret(&self) -> bool {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let offered = get_mime_types(self.paste_type(), Seat::Unspecified)
            .map(|types| types.contains(SECRET_HINT))
            .unwrap_or(false);
        offered && self.read(SECRET_HINT, 64).map_or(false, |v| String::from_utf8_lossy(&v).trim() == "secret")
    }

    fn set_secret(&self, value: &str) -> MyResult<()> {
        self.offer(vec![
            (value.as_bytes().to_vec(), CopyMimeType::Text),
            (b"secret".to_vec(), CopyMimeType::Specific(SECRET_HINT.to_string())),
        ])
    }

    fn clear(&self) -> MyResult<()> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        wl_copy::clear(self.copy_type(), wl_copy::Seat::All)?;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        match get_mime_types(self.paste_type(), Seat::Unspecified) {
            Ok(types) => types.is_empty(),
            Err(PasteError::NoSeats) | Err(PasteError::ClipboardEmpty) | Err(PasteError::NoMimeType) => true,
            Err(_) => false,
        }
    }

    fn primary(&self) -> bool {
        self.primary
    }

    fn changes(&self) -> Option<u64> {
        let w = self.watch.as_ref()?;
        // (no primary selection events from an older data-control: its text is compared instead)
        (!self.primary || w.primary_ok.load(std::sync::atomic::Ordering::SeqCst)).then(|| w.get(self.primary))
    }

    fn content(&self) -> MyResult<Content> {
        let mut c = Content { text: self.get()?, ..Default::default() };
        if self.primary {
            return Ok(c);
        }
        let Ok(types) = get_mime_types(self.paste_type(), Seat::Unspecified) else { return Ok(c) };
        c.secret = types.contains(SECRET_HINT)
            && self.read(SECRET_HINT, 64).map_or(false, |v| String::from_utf8_lossy(&v).trim() == "secret");
        if types.contains("text/uri-list") {
            c.files = file_list(self.read("text/uri-list", 1 << 20).unwrap_or_default());
        }
        if let Some(mime) = IMAGES.iter().find(|m| types.contains(**m)) {
            c.image = self.read(mime, MAX_IMAGE).filter(|d| !d.is_empty()).map(|d| (mime.to_string(), d));
        }
        Ok(c)
    }

    fn put(&self, c: &Content) -> MyResult<()> {
        if c.is_empty() {
            return self.clear();
        }
        let mut formats = vec![];
        if !c.text.is_empty() {
            formats.push((c.text.as_bytes().to_vec(), CopyMimeType::Text));
        }
        if !self.primary {
            if !c.files.is_empty() {
                formats.push((c.files.clone(), CopyMimeType::Specific("text/uri-list".into())));
                formats.push((c.gnome_files(), CopyMimeType::Specific("x-special/gnome-copied-files".into())));
            }
            if let Some((mime, data)) = &c.image {
                formats.push((data.clone(), CopyMimeType::Specific(mime.clone())));
            }
            if c.secret {
                formats.push((b"secret".to_vec(), CopyMimeType::Specific(SECRET_HINT.into())));
            }
        }
        if formats.is_empty() {
            return self.clear();
        }
        self.offer(formats)
    }

    fn primary_twin(&self) -> Option<Box<dyn Clipboard>> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let supported = wl_clipboard_rs::utils::is_primary_selection_supported().unwrap_or(false);
        (!self.primary && supported)
            .then(|| Box::new(WlrClipboard::new(self.display.clone(), true, self.watch.clone())) as Box<dyn Clipboard>)
    }
}

#[derive(Debug)]
pub struct WlCommandClipboard {
    pub display: String,
}

impl Clipboard for WlCommandClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        let out = Command::new("wl-paste")
            .env("WAYLAND_DISPLAY", &self.display)
            .output()?
            .stdout;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn set(&self, value: &str) -> MyResult<()> {
        Command::new("wl-copy")
            .arg(value)
            .env("WAYLAND_DISPLAY", &self.display)
            .spawn()?;
        Ok(())
    }

    fn should_poll(&self) -> bool {
        false
    }

    fn rank(&self) -> u8 {
        200
    }
}

#[derive(Debug)]
pub struct ArClipboard {
    display: String,
}

impl Clipboard for ArClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let mut clipboard = arboard::Clipboard::new()?;
        Ok(clipboard.get_text().unwrap_or_default())
    }

    fn set(&self, value: &str) -> MyResult<()> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let mut clipboard = arboard::Clipboard::new()?;
        clipboard.set_text(value.into())?;

        Ok(())
    }
}

pub struct X11Clipboard {
    display: String,
    primary: bool,
    backend: X11Backend,
    // (its own connection, to ask what's offered and in which formats: terminal_clipboard reads text only)
    marks: Option<x11_clipboard::Clipboard>,
    // (copies with several formats: text with a file list, a picture, the secret mark)
    owner: Option<Owner>,
    watch: Option<Arc<Counters>>,
}

#[derive(Clone)]
pub struct X11Backend(Rc<RefCell<terminal_clipboard::X11Clipboard>>);
impl X11Backend {
    /// try to only call this once because repeated initializations may not work.
    /// i started seeing timeouts/errors after 4
    pub fn new(display: &str) -> MyResult<Self> {
        // let backend = stdio! { terminal_clipboard::X11Clipboard::new() }.standardize()?;
        env::set_var("DISPLAY", display);
        let backend = terminal_clipboard::X11Clipboard::new().standardize()?;

        Ok(Self(Rc::new(RefCell::new(backend))))
    }
}

impl std::fmt::Debug for X11Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Clipboard")
            .field("display", &self.display)
            .field("primary", &self.primary)
            .finish()
    }
}

impl X11Clipboard {
    pub fn new(display: String, primary: bool, watch: Option<Arc<Counters>>) -> MyResult<Self> {
        let backend = X11Backend::new(&display)?;    // (sets DISPLAY to it, for the next one too)
        Ok(Self {
            backend,
            marks: x11_clipboard::Clipboard::new().ok(),
            owner: Owner::new(&display),
            primary,
            watch,
            display,
        })
    }

    /// With its change counter (the display's, shared with its primary selection's twin).
    pub fn watched(mut self, watch: Option<Arc<Counters>>) -> Self {
        self.watch = watch;
        self
    }

    fn selection(&self, c: &x11_clipboard::Clipboard) -> x11_clipboard::xcb::Atom {
        if self.primary { c.getter.atoms.primary } else { c.getter.atoms.clipboard }
    }

    /// The formats it's offered in now (Some([]): nothing owns it).
    fn offered(&self) -> Option<Vec<String>> {
        let c = self.marks.as_ref()?;
        owner::targets(&c.getter, self.selection(c))
    }

    /// One format's data (empty if it isn't offered after all).
    fn load(&self, format: &str, wait: Duration) -> Vec<u8> {
        let Some(c) = &self.marks else { return vec![] };
        let Ok(atom) = c.getter.get_atom(format) else { return vec![] };
        c.load(self.selection(c), atom, c.getter.atoms.property, wait).unwrap_or_default()
    }

    /// Own the selection with these formats (by name), or give it up (none).
    fn offer(&self, formats: Vec<(&str, Vec<u8>)>) -> MyResult<()> {
        let (Some(o), Some(c)) = (&self.owner, &self.marks) else {
            // (no owner of our own: text alone, as before)
            let text = formats.into_iter().find(|(f, _)| *f == "UTF8_STRING").map(|(_, v)| v).unwrap_or_default();
            self.backend.0.try_borrow_mut()?.set_string(&String::from_utf8_lossy(&text)).standardize()?;
            return Ok(());
        };
        let formats = formats.into_iter().map(|(f, v)| (o.atom(f), v)).collect();
        o.own(self.selection(c), formats);
        Ok(())
    }

    fn text_formats(text: &str) -> Vec<(&'static str, Vec<u8>)> {
        ["UTF8_STRING", "text/plain;charset=utf-8", "text/plain", "STRING", "TEXT"]
            .into_iter()
            .map(|f| (f, text.as_bytes().to_vec()))
            .collect()
    }
}

impl Clipboard for X11Clipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        if self.primary {
            return Ok(String::from_utf8_lossy(&self.load("UTF8_STRING", Duration::from_millis(500))).to_string());
        }
        Ok(self
            .backend
            .0
            .try_borrow()?
            .get_string()
            .unwrap_or_default())
    }

    fn set(&self, value: &str) -> MyResult<()> {
        self.offer(Self::text_formats(value))
    }

    fn is_secret(&self) -> bool {
        !self.primary
            && self.offered().map_or(false, |f| f.iter().any(|t| t == SECRET_HINT))
            && String::from_utf8_lossy(&self.load(SECRET_HINT, Duration::from_millis(300))).trim() == "secret"
    }

    fn set_secret(&self, value: &str) -> MyResult<()> {
        let mut formats = Self::text_formats(value);
        formats.push((SECRET_HINT, b"secret".to_vec()));
        self.offer(formats)
    }

    fn clear(&self) -> MyResult<()> {
        self.offer(vec![])
    }

    fn is_empty(&self) -> bool {
        self.offered().map_or(false, |f| f.is_empty())
    }

    fn primary(&self) -> bool {
        self.primary
    }

    fn changes(&self) -> Option<u64> {
        self.watch.as_ref().map(|w| w.get(self.primary))
    }

    fn content(&self) -> MyResult<Content> {
        let mut c = Content { text: self.get()?, ..Default::default() };
        if self.primary {
            return Ok(c);
        }
        let offered = self.offered().unwrap_or_default();
        let has = |f: &str| offered.iter().any(|t| t == f);
        c.secret = has(SECRET_HINT) && String::from_utf8_lossy(&self.load(SECRET_HINT, Duration::from_millis(300))).trim() == "secret";
        if has("text/uri-list") {
            c.files = file_list(self.load("text/uri-list", Duration::from_millis(500)));
        }
        if let Some(mime) = IMAGES.iter().find(|m| has(m)) {
            let data = self.load(mime, Duration::from_secs(3));
            if !data.is_empty() && data.len() <= MAX_IMAGE {
                c.image = Some((mime.to_string(), data));
            }
        }
        Ok(c)
    }

    fn put(&self, c: &Content) -> MyResult<()> {
        let mut formats = if c.text.is_empty() { vec![] } else { Self::text_formats(&c.text) };
        if !self.primary {
            if !c.files.is_empty() {
                formats.push(("text/uri-list", c.files.clone()));
                formats.push(("x-special/gnome-copied-files", c.gnome_files()));
            }
            if let Some((mime, data)) = &c.image {
                formats.push((mime.as_str(), data.clone()));
            }
            if c.secret {
                formats.push((SECRET_HINT, b"secret".to_vec()));
            }
        }
        self.offer(formats)
    }

    fn primary_twin(&self) -> Option<Box<dyn Clipboard>> {
        if self.primary {
            return None;
        }
        X11Clipboard::new(self.display.clone(), true, self.watch.clone())
            .ok()
            .map(|c| Box::new(c) as Box<dyn Clipboard>)
    }
}

#[derive(Debug)]
pub struct HybridClipboard<G: Clipboard, S: Clipboard> {
    getter: G,
    setter: S,
}

// impl HybridClipboard<X11Clipboard, CommandClipboard> {
//     fn gnome(n: u8) -> MyResult<Self> {
//         Ok(Self {
//             getter: X11Clipboard::new(format!(":{}", n))?,
//             setter: CommandClipboard {
//                 display: format!("wayland-{}", n),
//             },
//         })
//     }
// }

impl<G: Clipboard, S: Clipboard> Clipboard for HybridClipboard<G, S> {
    fn display(&self) -> String {
        self.getter.display()
    }

    fn get(&self) -> MyResult<String> {
        self.getter.get()
    }

    fn set(&self, value: &str) -> MyResult<()> {
        self.setter.set(value)
    }
}
