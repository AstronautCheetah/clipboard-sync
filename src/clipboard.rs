use std::cell::RefCell;
use std::rc::Rc;
use std::{env, io::Read, process::Command};
use terminal_clipboard::Clipboard as TerminalClipboard;
use wl_clipboard_rs::copy::{MimeType as CopyMimeType, Options, Source};
use wl_clipboard_rs::paste::{
    get_contents, get_mime_types, ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat,
};

use crate::error::{Generify, MyResult, Standardize};

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
}

#[derive(Debug)]
pub struct WlrClipboard {
    pub display: String,
}

impl Clipboard for WlrClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        // Plain text only. wl-clipboard-rs counts any text/* as text, so an image copied with an
        // HTML <img> beside it (for apps that want one) would spread as markup and replace the image.
        if let Ok(types) = get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            let plain = |t: &String| t.starts_with("text/plain") || ["UTF8_STRING", "STRING", "TEXT"].contains(&t.as_str());
            if !types.iter().any(plain) {
                return Ok("".to_string());
            }
        }
        let result = get_contents(
            ClipboardType::Regular,
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
        env::set_var("WAYLAND_DISPLAY", self.display.clone());
        let opts = Options::new();
        let result = std::panic::catch_unwind(|| {
            opts.copy(
                Source::Bytes(value.to_string().into_bytes().into()),
                CopyMimeType::Text,
            )
        });

        Ok(result.standardize().generify()??)
    }

    fn rank(&self) -> u8 {
        10
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
    backend: X11Backend,
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
            .finish()
    }
}

impl X11Clipboard {
    pub fn new(display: String) -> MyResult<Self> {
        Ok(Self {
            backend: X11Backend::new(&display)?,
            display,
        })
    }

    // fn backend(&self) -> X11Backend {
    //     X11Backend::new_str(&self.display).unwrap()
    // }
}

impl Clipboard for X11Clipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get(&self) -> MyResult<String> {
        Ok(self
            .backend
            .0
            .try_borrow()?
            .get_string()
            .unwrap_or_default())
    }

    fn set(&self, value: &str) -> MyResult<()> {
        self.backend
            .0
            .try_borrow_mut()?
            .set_string(value)
            .standardize()?;

        Ok(())
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
