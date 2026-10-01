//! The terminal view as an accessibility text area (macOS) — an OS seam
//! (design §7).
//!
//! winit's `WinitView` exposes nothing to the macOS Accessibility API, so the
//! focused element an assistive client sees in an Ember window is the bare
//! `AXWindow`. Dictation tools (Speechify and friends) look for a focused text
//! element they can write to and, finding none, fall back to "copy the text
//! yourself". iTerm2's terminal view is a focused `AXTextArea` whose `AXValue`
//! and `AXSelectedTextRange` are settable; this module makes Ember's match.
//!
//! The NSAccessibility methods are added to winit's view class at runtime with
//! `class_addMethod`, and only ever ADDED: if `WinitView` already implements
//! any of them itself (a future winit release), nothing is installed and the
//! view keeps winit's behavior. They override NSView's inherited defaults,
//! which is what an accessibility-aware NSView subclass does anyway.
//!
//! The write path is deliberately narrow. Setting `AXValue` pastes the text the
//! client inserted (the new value minus what we last served it) into the
//! focused pane through the app's normal paste path, bracketed-paste included;
//! setting `AXSelectedTextRange` is accepted and ignored (a terminal has no
//! client-movable caret). Nothing else about the terminal's text changes.
//!
//! Main-thread only: AppKit calls these on the main thread, and the app owns
//! the per-window handles there too. Requests are queued and the event loop is
//! woken; the app drains them with [`drain`].

use std::sync::Arc;

/// Something an accessibility client asked of a terminal view, for the app to
/// carry out on its next loop turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AxRequest {
    /// The client wrote text into the view: paste it into the focused pane.
    Insert { view: usize, text: String },
    /// A client started reading the view: start keeping its text current.
    Refresh { view: usize },
}

/// The text a client inserted, given the value it last read (`old`) and the
/// value it wrote back (`new`): whatever lies between their common prefix and
/// common suffix in `new`. Characters the client removed are ignored (the
/// terminal's text is read-only), so a pure deletion inserts nothing.
pub fn inserted_text(old: &str, new: &str) -> String {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(_, b)| b.len_utf8())
        .sum();
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix: usize = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(_, b)| b.len_utf8())
        .sum();
    new_rest[..new_rest.len() - suffix].to_string()
}

/// One window's registration as an accessibility text area. Dropping it
/// unregisters the view (it then answers as a plain, non-element NSView).
pub struct AxTextArea {
    view: usize,
}

impl AxTextArea {
    /// Make `window`'s view an accessibility text area. `None` off macOS, or
    /// when the methods could not be installed (see the module docs).
    pub fn install(
        window: &winit::window::Window,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Self> {
        imp::install(window, wake).map(|view| Self { view })
    }

    /// The registered view's key, as carried by [`AxRequest`].
    pub fn view(&self) -> usize {
        self.view
    }

    /// Whether a client has read this view, i.e. whether the app should keep
    /// its text current with [`set_text`](Self::set_text).
    pub fn active(&self) -> bool {
        imp::active(self.view)
    }

    /// Publish the view's current text: the focused pane's screen, rows joined
    /// by `\n`, with the cursor at (`row`, `col`) (`col` in chars).
    pub fn set_text(&self, text: String, row: u16, col: u16) {
        imp::set_text(self.view, text, row, col);
    }
}

impl Drop for AxTextArea {
    fn drop(&mut self) {
        imp::unregister(self.view);
    }
}

/// Take every queued request (call from the event loop on the main thread).
pub fn drain() -> Vec<AxRequest> {
    imp::drain()
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::AxRequest;
    use std::sync::Arc;

    pub fn install(_: &winit::window::Window, _: Arc<dyn Fn() + Send + Sync>) -> Option<usize> {
        None
    }
    pub fn active(_: usize) -> bool {
        false
    }
    pub fn set_text(_: usize, _: String, _: u16, _: u16) {}
    pub fn unregister(_: usize) {}
    pub fn drain() -> Vec<AxRequest> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod imp {
    use super::{AxRequest, inserted_text};
    use objc2::encode::{EncodeArguments, EncodeReturn, Encoding};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::{ClassType, msg_send, sel};
    use objc2_foundation::{NSInteger, NSRange, NSString};
    use std::cell::{OnceCell, RefCell};
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::sync::Arc;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[derive(Default)]
    struct ViewState {
        /// The latest published screen text.
        text: String,
        /// What we last answered `accessibilityValue` with: the base a
        /// client's `setAccessibilityValue:` is diffed against.
        served: Option<String>,
        cursor_row: u16,
        cursor_col: u16,
        /// A client has read this view; the app keeps `text` current.
        active: bool,
    }

    thread_local! {
        static VIEWS: RefCell<HashMap<usize, ViewState>> = RefCell::new(HashMap::new());
        static QUEUE: RefCell<Vec<AxRequest>> = const { RefCell::new(Vec::new()) };
        static WAKE: OnceCell<Arc<dyn Fn() + Send + Sync>> = const { OnceCell::new() };
        /// Per view class: whether our methods are installed on it.
        static INSTALLED: RefCell<HashMap<usize, bool>> = RefCell::new(HashMap::new());
    }

    fn with_view<R>(view: usize, f: impl FnOnce(&mut ViewState) -> R) -> Option<R> {
        VIEWS.with(|v| v.borrow_mut().get_mut(&view).map(f))
    }

    fn push(req: AxRequest) {
        QUEUE.with(|q| q.borrow_mut().push(req));
        WAKE.with(|w| {
            if let Some(w) = w.get() {
                w();
            }
        });
    }

    pub fn install(
        window: &winit::window::Window,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<usize> {
        let RawWindowHandle::AppKit(h) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        let view = h.ns_view.as_ptr() as usize;
        // SAFETY: winit's AppKit handle is a live NSView for the window's life.
        let obj: &AnyObject = unsafe { &*(view as *const AnyObject) };
        if !install_on(obj.class()) {
            return None;
        }
        WAKE.with(|w| {
            let _ = w.set(wake);
        });
        VIEWS.with(|v| v.borrow_mut().insert(view, ViewState::default()));
        Some(view)
    }

    pub fn active(view: usize) -> bool {
        with_view(view, |s| s.active).unwrap_or(false)
    }

    pub fn set_text(view: usize, text: String, row: u16, col: u16) {
        with_view(view, |s| {
            s.text = text;
            s.cursor_row = row;
            s.cursor_col = col;
        });
    }

    pub fn unregister(view: usize) {
        VIEWS.with(|v| v.borrow_mut().remove(&view));
    }

    pub fn drain() -> Vec<AxRequest> {
        QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()))
    }

    /// One method to add: selector, implementation, type encoding.
    struct Method(Sel, Imp, CString);

    fn method<R: EncodeReturn, A: EncodeArguments>(sel: Sel, imp: Imp) -> Method {
        let mut types = format!(
            "{}{}{}",
            R::ENCODING_RETURN,
            Encoding::Object,
            Encoding::Sel
        );
        for e in A::ENCODINGS {
            types.push_str(&e.to_string());
        }
        Method(sel, imp, CString::new(types).expect("encoding has no NUL"))
    }

    /// Add the accessibility methods to `cls` (once per class). Returns whether
    /// they are in place. Refuses, installing nothing, when the class itself
    /// already implements any of them: we add, never replace.
    fn install_on(cls: &AnyClass) -> bool {
        let key = cls as *const AnyClass as usize;
        if let Some(done) = INSTALLED.with(|i| i.borrow().get(&key).copied()) {
            return done;
        }
        // SAFETY (all transmutes): each fn below is `extern "C-unwind"` with
        // the receiver, `_cmd`, then the arguments its encoding declares; Imp
        // is the erased form the runtime calls through.
        #[allow(clippy::missing_transmute_annotations)]
        let methods = unsafe {
            use std::mem::transmute as t;
            [
                method::<Bool, ()>(
                    sel!(isAccessibilityElement),
                    t(is_element as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<Bool, ()>(
                    sel!(isAccessibilityFocused),
                    t(is_focused as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<*mut NSString, ()>(
                    sel!(accessibilityRole),
                    t(role as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<*mut AnyObject, ()>(
                    sel!(accessibilityValue),
                    t(value as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<(), (*mut AnyObject,)>(
                    sel!(setAccessibilityValue:),
                    t(set_value as extern "C-unwind" fn(_, _, _)),
                ),
                method::<*mut NSString, ()>(
                    sel!(accessibilitySelectedText),
                    t(selected_text as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<NSRange, ()>(
                    sel!(accessibilitySelectedTextRange),
                    t(selected_range as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<(), (NSRange,)>(
                    sel!(setAccessibilitySelectedTextRange:),
                    t(set_selected_range as extern "C-unwind" fn(_, _, _)),
                ),
                method::<NSRange, ()>(
                    sel!(accessibilityVisibleCharacterRange),
                    t(visible_range as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<NSInteger, ()>(
                    sel!(accessibilityNumberOfCharacters),
                    t(char_count as extern "C-unwind" fn(_, _) -> _),
                ),
                method::<NSInteger, ()>(
                    sel!(accessibilityInsertionPointLineNumber),
                    t(insertion_line as extern "C-unwind" fn(_, _) -> _),
                ),
            ]
        };
        let own: Vec<Sel> = cls.instance_methods().iter().map(|m| m.name()).collect();
        let clash: Vec<&str> = methods
            .iter()
            .filter(|m| own.contains(&m.0))
            .map(|m| m.0.name().to_str().unwrap_or("?"))
            .collect();
        let ok = if clash.is_empty() {
            methods.iter().all(|Method(sel, imp, types)| {
                // SAFETY: `imp` matches `types` (built from the same signature).
                unsafe {
                    objc2::ffi::class_addMethod(
                        cls as *const _ as *mut _,
                        *sel,
                        *imp,
                        types.as_ptr(),
                    )
                }
                .as_bool()
            })
        } else {
            eprintln!(
                "[ember] accessibility: {} already implements {clash:?}; leaving it as is",
                cls.name().to_str().unwrap_or("?")
            );
            false
        };
        INSTALLED.with(|i| i.borrow_mut().insert(key, ok));
        ok
    }

    fn key(this: &AnyObject) -> usize {
        this as *const AnyObject as usize
    }

    fn registered(this: &AnyObject) -> bool {
        with_view(key(this), |_| ()).is_some()
    }

    /// Mark the view read; the first read asks the app for current text.
    fn touch(this: &AnyObject) {
        let first =
            with_view(key(this), |s| !std::mem::replace(&mut s.active, true)).unwrap_or(false);
        if first {
            push(AxRequest::Refresh { view: key(this) });
        }
    }

    fn utf16_len(s: &str) -> usize {
        s.encode_utf16().count()
    }

    /// The cursor's offset into the text, in UTF-16 units (AppKit's index).
    fn cursor_offset(s: &ViewState) -> usize {
        let mut off = 0;
        for (i, line) in s.text.split('\n').enumerate() {
            if i == s.cursor_row as usize {
                return off
                    + line
                        .chars()
                        .take(s.cursor_col as usize)
                        .map(char::len_utf16)
                        .sum::<usize>();
            }
            off += utf16_len(line) + 1;
        }
        utf16_len(&s.text)
    }

    fn ns_string(s: &str) -> *mut NSString {
        Retained::autorelease_ptr(NSString::from_str(s))
    }

    extern "C-unwind" fn is_element(this: &AnyObject, _: Sel) -> Bool {
        Bool::new(registered(this))
    }

    extern "C-unwind" fn is_focused(this: &AnyObject, _: Sel) -> Bool {
        if !registered(this) {
            return Bool::NO;
        }
        // SAFETY: `window` and `firstResponder` are plain getters on a live
        // NSView / NSWindow (nil-safe).
        let first: *mut AnyObject = unsafe {
            let window: *mut AnyObject = msg_send![this, window];
            if window.is_null() {
                return Bool::NO;
            }
            msg_send![window, firstResponder]
        };
        Bool::new(std::ptr::eq(first, this))
    }

    extern "C-unwind" fn role(this: &AnyObject, _: Sel) -> *mut NSString {
        ns_string(if registered(this) {
            "AXTextArea"
        } else {
            "AXGroup"
        })
    }

    extern "C-unwind" fn value(this: &AnyObject, _: Sel) -> *mut AnyObject {
        touch(this);
        let text = with_view(key(this), |s| {
            s.served = Some(s.text.clone());
            s.text.clone()
        });
        match text {
            Some(t) => ns_string(&t).cast(),
            None => std::ptr::null_mut(),
        }
    }

    extern "C-unwind" fn set_value(this: &AnyObject, _: Sel, new: *mut AnyObject) {
        // SAFETY: a client passes an NSString (or an NSAttributedString,
        // whose `string` is one); anything else is ignored.
        let new = unsafe {
            let Some(obj) = new.as_ref() else { return };
            let s: *mut NSString = if msg_send![obj, isKindOfClass: NSString::class()] {
                new.cast()
            } else if msg_send![obj, respondsToSelector: sel!(string)] {
                msg_send![obj, string]
            } else {
                return;
            };
            match s.as_ref() {
                Some(s) => s.to_string(),
                None => return,
            }
        };
        let Some(text) = with_view(key(this), |s| {
            let base = s.served.clone().unwrap_or_else(|| s.text.clone());
            let text = inserted_text(&base, &new);
            // The client's view of the value now includes its insertion.
            s.served = Some(new);
            text
        }) else {
            return;
        };
        if !text.is_empty() {
            push(AxRequest::Insert {
                view: key(this),
                text,
            });
        }
    }

    extern "C-unwind" fn selected_text(this: &AnyObject, _: Sel) -> *mut NSString {
        touch(this);
        ns_string("")
    }

    extern "C-unwind" fn selected_range(this: &AnyObject, _: Sel) -> NSRange {
        touch(this);
        NSRange::new(with_view(key(this), |s| cursor_offset(s)).unwrap_or(0), 0)
    }

    /// Accepted (so the attribute reads as settable, like iTerm2's) and
    /// ignored: the terminal's caret belongs to the program running in it.
    extern "C-unwind" fn set_selected_range(_: &AnyObject, _: Sel, _: NSRange) {}

    extern "C-unwind" fn visible_range(this: &AnyObject, _: Sel) -> NSRange {
        NSRange::new(0, with_view(key(this), |s| utf16_len(&s.text)).unwrap_or(0))
    }

    extern "C-unwind" fn char_count(this: &AnyObject, _: Sel) -> NSInteger {
        touch(this);
        with_view(key(this), |s| utf16_len(&s.text)).unwrap_or(0) as NSInteger
    }

    extern "C-unwind" fn insertion_line(this: &AnyObject, _: Sel) -> NSInteger {
        touch(this);
        with_view(key(this), |s| s.cursor_row).unwrap_or(0) as NSInteger
    }
}

#[cfg(test)]
mod tests {
    use super::inserted_text;

    #[test]
    fn append_at_end() {
        assert_eq!(inserted_text("$ ls", "$ ls -la"), " -la");
    }

    #[test]
    fn insert_in_middle() {
        assert_eq!(inserted_text("a\n$ \nb", "a\n$ hello\nb"), "hello");
    }

    #[test]
    fn from_empty_value() {
        assert_eq!(inserted_text("", "dictated text"), "dictated text");
    }

    #[test]
    fn replacement_keeps_only_the_new_text() {
        assert_eq!(inserted_text("say foo now", "say barbaz now"), "barbaz");
    }

    #[test]
    fn pure_deletion_inserts_nothing() {
        assert_eq!(inserted_text("abc", "ac"), "");
    }

    #[test]
    fn repeated_chars_and_multibyte() {
        assert_eq!(inserted_text("aa", "aaa"), "a");
        assert_eq!(inserted_text("é$", "é café$"), " café");
    }
}
