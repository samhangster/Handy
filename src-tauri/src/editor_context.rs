//! Insertion context. Editor text is only read, used transiently, and never logged or persisted.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
    pid: i32,
    window: usize,
    field: usize,
}

/// Safe to carry to a paste completion callback: no native pointers or editor contents.
#[derive(Clone, Debug)]
pub struct Snapshot {
    target: Option<Target>,
    boundary: Option<bool>,
    selection: Option<(usize, usize)>,
    text_length: Option<usize>,
    role: String,
    source: &'static str,
    status: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            target: None,
            boundary: None,
            selection: None,
            text_length: None,
            role: String::new(),
            source: "unavailable",
            status: "unsupported platform".into(),
        }
    }
}

#[derive(Clone)]
struct Continuation {
    target: Target,
    boundary: bool,
    expected_cursor: Option<usize>,
    updated: Instant,
}

#[derive(Default)]
struct Continuations(VecDeque<Continuation>);

impl Continuations {
    fn lookup(&self, snapshot: &Snapshot, now: Instant) -> Option<bool> {
        let target = snapshot.target?;
        self.0.iter().rev().find_map(|entry| {
            if now.saturating_duration_since(entry.updated) > Duration::from_secs(15 * 60)
                || entry.target != target
            {
                return None;
            }
            if let Some((location, selected)) = snapshot.selection {
                if selected != 0
                    || entry
                        .expected_cursor
                        .is_some_and(|expected| location != expected)
                {
                    return None;
                }
            }
            Some(entry.boundary)
        })
    }

    fn commit(&mut self, snapshot: &Snapshot, output: &str, now: Instant) {
        let Some(target) = snapshot.target else {
            return;
        };
        if output.is_empty() {
            return;
        }
        self.0.retain(|entry| entry.target != target);
        self.0.push_back(Continuation {
            target,
            boundary: crate::text_formatting::ends_at_sentence_boundary(output),
            expected_cursor: snapshot
                .selection
                .and_then(|(location, _)| location.checked_add(output.encode_utf16().count())),
            updated: now,
        });
        while self.0.len() > 64 {
            self.0.pop_front();
        }
    }
}

static CONTINUATIONS: Mutex<Continuations> = Mutex::new(Continuations(VecDeque::new()));

impl Snapshot {
    pub fn capitalization(&self) -> Option<bool> {
        self.boundary
            .or_else(|| CONTINUATIONS.lock().ok()?.lookup(self, Instant::now()))
    }

    /// Deliberately excludes values, window titles, identifiers and selected text.
    pub fn diagnostic(&self) -> serde_json::Value {
        serde_json::json!({
            "pid": self.target.map(|target| target.pid),
            "role": self.role,
            "source": self.source,
            "status": self.status,
            "selection_location": self.selection.map(|range| range.0),
            "selection_length": self.selection.map(|range| range.1),
            "text_length": self.text_length,
            "native_capitalization": self.boundary,
            "resolved_capitalization": self.capitalization(),
        })
    }
}

/// Call only after a successful insertion/receipt, never for previews or history retries.
pub fn commit(snapshot: &Snapshot, output: &str) {
    if let Ok(mut state) = CONTINUATIONS.lock() {
        state.commit(snapshot, output, Instant::now());
    }
}

fn capitalize_prefix(prefix: &str) -> bool {
    prefix.trim().is_empty() || crate::text_formatting::ends_at_sentence_boundary(prefix)
}

/// Ask supported apps to expose their accessibility tree early in dictation.
/// Electron may take two seconds to enable it; this function never waits for it.
#[cfg(not(target_os = "macos"))]
pub fn prepare() {}

#[cfg(target_os = "macos")]
pub fn prepare() {
    native::prepare();
}

#[cfg(not(target_os = "macos"))]
pub fn capture() -> Snapshot {
    Snapshot::default()
}

#[cfg(target_os = "macos")]
pub fn capture() -> Snapshot {
    native::capture()
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ffi::{c_char, c_void, CStr};
    type Ref = *const c_void;
    const RANGE_TYPE: u32 = 4;
    const MAX_TEXT: usize = 1_000_000;
    const CAPTURE_TIMEOUT: Duration = Duration::from_millis(600);
    const REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
    const AX_CANNOT_COMPLETE: i32 = -25204;
    const AX_ATTRIBUTE_UNSUPPORTED: i32 = -25205;

    #[derive(Default)]
    struct ActivationRequests(Vec<i32>);

    impl ActivationRequests {
        fn reserve(&mut self, pid: i32) -> bool {
            if pid <= 0 || self.0.contains(&pid) {
                return false;
            }
            self.0.push(pid);
            true
        }

        fn finish(&mut self, pid: i32, completed: bool) {
            // Failed IPC may be retried on capture. Keep successful requests and
            // unsupported apps cached: repeated true requests reset Electron's
            // two-second debounce and can prevent activation indefinitely.
            if !completed {
                self.0.retain(|requested| *requested != pid);
            }
        }
    }

    static ACTIVATION_REQUESTS: Mutex<ActivationRequests> =
        Mutex::new(ActivationRequests(Vec::new()));

    #[derive(Clone, Copy)]
    struct CaptureBudget {
        deadline: Instant,
        cannot_complete: bool,
    }

    impl CaptureBudget {
        fn request_timeout(self, now: Instant) -> Option<Duration> {
            if self.cannot_complete {
                return None;
            }
            let remaining = self.deadline.checked_duration_since(now)?;
            // A zero AX timeout means "use the default", so never pass it.
            (remaining >= Duration::from_millis(1)).then_some(remaining.min(REQUEST_TIMEOUT))
        }
    }

    thread_local! {
        static CAPTURE_BUDGET: std::cell::Cell<Option<CaptureBudget>> = const {
            std::cell::Cell::new(None)
        };
    }

    struct BudgetGuard(Option<CaptureBudget>);
    impl BudgetGuard {
        fn start() -> Self {
            Self(CAPTURE_BUDGET.with(|budget| {
                budget.replace(Some(CaptureBudget {
                    deadline: Instant::now() + CAPTURE_TIMEOUT,
                    cannot_complete: false,
                }))
            }))
        }
    }
    impl Drop for BudgetGuard {
        fn drop(&mut self) {
            CAPTURE_BUDGET.with(|budget| budget.set(self.0));
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Range {
        location: isize,
        length: isize,
    }

    #[repr(C)]
    struct ArrayCallbacks {
        version: isize,
        retain: Option<unsafe extern "C" fn(Ref, Ref) -> Ref>,
        release: Option<unsafe extern "C" fn(Ref, Ref)>,
        copy_description: Option<unsafe extern "C" fn(Ref) -> Ref>,
        equal: Option<unsafe extern "C" fn(Ref, Ref) -> bool>,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> Ref;
        fn AXIsProcessTrusted() -> bool;
        fn AXUIElementCopyAttributeValue(element: Ref, attribute: Ref, value: *mut Ref) -> i32;
        fn AXUIElementIsAttributeSettable(element: Ref, attribute: Ref, settable: *mut bool)
            -> i32;
        fn AXUIElementSetAttributeValue(element: Ref, attribute: Ref, value: Ref) -> i32;
        fn AXUIElementCopyParameterizedAttributeValue(
            element: Ref,
            attribute: Ref,
            parameter: Ref,
            value: *mut Ref,
        ) -> i32;
        fn AXUIElementGetPid(element: Ref, pid: *mut i32) -> i32;
        fn AXUIElementSetMessagingTimeout(element: Ref, seconds: f32) -> i32;
        fn AXValueCreate(kind: u32, value: *const c_void) -> Ref;
        fn AXValueGetValue(value: Ref, kind: u32, output: *mut c_void) -> bool;
        fn AXValueGetType(value: Ref) -> u32;
        fn AXValueGetTypeID() -> usize;
        fn AXTextMarkerRangeGetTypeID() -> usize;
        fn AXTextMarkerRangeCopyStartMarker(range: Ref) -> Ref;
        fn AXTextMarkerRangeCopyEndMarker(range: Ref) -> Ref;
        fn AXTextMarkerRangeCreate(allocator: Ref, start: Ref, end: Ref) -> Ref;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(value: Ref);
        fn CFRetain(value: Ref) -> Ref;
        fn CFHash(value: Ref) -> usize;
        fn CFEqual(left: Ref, right: Ref) -> bool;
        fn CFGetTypeID(value: Ref) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: Ref) -> bool;
        static kCFBooleanTrue: Ref;
        fn CFStringGetTypeID() -> usize;
        fn CFStringCreateWithCString(allocator: Ref, bytes: *const c_char, encoding: u32) -> Ref;
        fn CFStringGetLength(value: Ref) -> isize;
        fn CFStringGetCharacters(value: Ref, range: Range, buffer: *mut u16);
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(value: Ref, kind: isize, output: *mut c_void) -> bool;
        fn CFArrayGetTypeID() -> usize;
        fn CFArrayGetCount(value: Ref) -> isize;
        fn CFArrayCreate(
            allocator: Ref,
            values: *const Ref,
            count: isize,
            callbacks: *const ArrayCallbacks,
        ) -> Ref;
        static kCFTypeArrayCallBacks: ArrayCallbacks;
    }
    struct Owned(Ref);
    impl Owned {
        unsafe fn new(value: Ref) -> Option<Self> {
            (!value.is_null()).then(|| Self(value))
        }
        unsafe fn retained(value: Ref) -> Self {
            Self(CFRetain(value))
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }

    unsafe fn key(name: &CStr) -> Option<Owned> {
        Owned::new(CFStringCreateWithCString(
            std::ptr::null(),
            name.as_ptr(),
            0x08000100,
        ))
    }
    unsafe fn prepare_request(element: Ref) -> Option<()> {
        let timeout =
            CAPTURE_BUDGET.with(|budget| budget.get()?.request_timeout(Instant::now()))?;
        // Apply the remaining budget to every element, including newly traversed
        // ancestors. A per-call timeout alone does not bound the whole capture.
        AXUIElementSetMessagingTimeout(element, timeout.as_secs_f32());
        Some(())
    }
    fn record_status(status: i32) {
        if status == AX_CANNOT_COMPLETE {
            CAPTURE_BUDGET.with(|budget| {
                if let Some(mut current) = budget.get() {
                    current.cannot_complete = true;
                    budget.set(Some(current));
                }
            });
        }
    }
    unsafe fn attribute(element: Ref, name: &CStr) -> Option<Owned> {
        prepare_request(element)?;
        let name = key(name)?;
        let mut value = std::ptr::null();
        let status = AXUIElementCopyAttributeValue(element, name.0, &mut value);
        record_status(status);
        let value = Owned::new(value);
        if status == 0 {
            value
        } else {
            None
        }
    }
    unsafe fn parameter(element: Ref, name: &CStr, argument: Ref) -> Option<Owned> {
        prepare_request(element)?;
        let name = key(name)?;
        let mut value = std::ptr::null();
        let status =
            AXUIElementCopyParameterizedAttributeValue(element, name.0, argument, &mut value);
        record_status(status);
        let value = Owned::new(value);
        if status == 0 {
            value
        } else {
            None
        }
    }

    unsafe fn application_pid(application: Ref) -> Option<i32> {
        prepare_request(application)?;
        let mut pid = 0;
        let status = AXUIElementGetPid(application, &mut pid);
        record_status(status);
        (status == 0 && pid > 0).then_some(pid)
    }

    unsafe fn request_manual_accessibility(application: Ref) -> Option<()> {
        // Reading the application role activates native accessibility in recent
        // Chromium, but querying focused UI alone does not activate web content.
        let _ = attribute(application, c"AXRole");
        let name = key(c"AXManualAccessibility")?;
        prepare_request(application)?;
        let mut settable = false;
        let status = AXUIElementIsAttributeSettable(application, name.0, &mut settable);
        record_status(status);
        if status == AX_ATTRIBUTE_UNSUPPORTED || (status == 0 && !settable) {
            return Some(());
        }
        if status != 0 {
            return None;
        }
        // This is Electron's documented opt-in attribute. Only apps declaring
        // support are changed; do not enable undocumented modes in arbitrary apps.
        if attribute(application, c"AXManualAccessibility").is_some_and(|value| {
            CFGetTypeID(value.0) == CFBooleanGetTypeID() && CFBooleanGetValue(value.0)
        }) {
            return Some(());
        }
        prepare_request(application)?;
        let status = AXUIElementSetAttributeValue(application, name.0, kCFBooleanTrue);
        record_status(status);
        (status == 0).then_some(())
    }

    unsafe fn prepare_application(application: Ref, pid: i32) {
        // Reserve before IPC so simultaneous preparation and capture cannot
        // send duplicate requests. No mutex is held while another app responds.
        let reserved = ACTIVATION_REQUESTS
            .lock()
            .map(|mut requests| requests.reserve(pid))
            .unwrap_or(false);
        if !reserved {
            return;
        }
        let completed = request_manual_accessibility(application).is_some();
        if let Ok(mut requests) = ACTIVATION_REQUESTS.lock() {
            requests.finish(pid, completed);
        }
    }

    pub fn prepare() {
        let _guard = BudgetGuard::start();
        unsafe {
            if !AXIsProcessTrusted() {
                return;
            }
            let Some(system) = Owned::new(AXUIElementCreateSystemWide()) else {
                return;
            };
            let Some(application) = attribute(system.0, c"AXFocusedApplication") else {
                return;
            };
            if let Some(pid) = application_pid(application.0) {
                prepare_application(application.0, pid);
            }
        }
    }
    unsafe fn string(value: Ref) -> Option<String> {
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        let length = CFStringGetLength(value);
        if length < 0 || length as usize > MAX_TEXT {
            return None;
        }
        let mut units = vec![0; length as usize];
        CFStringGetCharacters(
            value,
            Range {
                location: 0,
                length,
            },
            units.as_mut_ptr(),
        );
        String::from_utf16(&units).ok()
    }
    unsafe fn text(element: Ref, name: &CStr) -> Option<String> {
        string(attribute(element, name)?.0)
    }
    unsafe fn number(value: Ref) -> Option<usize> {
        if CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut integer: i64 = 0;
        if !CFNumberGetValue(value, 4, (&mut integer as *mut i64).cast()) {
            return None;
        }
        usize::try_from(integer).ok()
    }
    unsafe fn selection(element: Ref) -> Option<(usize, usize)> {
        let value = attribute(element, c"AXSelectedTextRange")?;
        if CFGetTypeID(value.0) != AXValueGetTypeID() || AXValueGetType(value.0) != RANGE_TYPE {
            return None;
        }
        let mut range = Range {
            location: 0,
            length: 0,
        };
        if !AXValueGetValue(value.0, RANGE_TYPE, (&mut range as *mut Range).cast()) {
            return None;
        }
        Some((
            usize::try_from(range.location).ok()?,
            usize::try_from(range.length).ok()?,
        ))
    }
    unsafe fn range_text(element: Ref, location: usize, length: usize) -> Option<String> {
        if location.checked_add(length)? > MAX_TEXT {
            return None;
        }
        let range = Range {
            location: location as isize,
            length: length as isize,
        };
        let argument = Owned::new(AXValueCreate(RANGE_TYPE, (&range as *const Range).cast()))?;
        let value = parameter(element, c"AXStringForRange", argument.0)?;
        let result = string(value.0)?;
        (result.encode_utf16().count() == length).then_some(result)
    }
    fn editable(role: &str) -> bool {
        matches!(role, "AXTextArea" | "AXTextField" | "AXComboBox")
    }

    unsafe fn child_count(element: Ref) -> Option<usize> {
        let children = attribute(element, c"AXChildren")?;
        if CFGetTypeID(children.0) != CFArrayGetTypeID() {
            return None;
        }
        usize::try_from(CFArrayGetCount(children.0)).ok()
    }

    unsafe fn ordinary_prefix(
        field: Ref,
        selection: (usize, usize),
        count: Option<usize>,
        value: Option<&str>,
        plain: bool,
    ) -> Option<(String, &'static str)> {
        let (location, selected) = selection;
        let end = location.checked_add(selected)?;
        if end > MAX_TEXT || count.is_some_and(|count| end > count) {
            return None;
        }
        if location == 0 {
            // A zero range is a common unsupported/stale rich-editor default.
            // Only ordinary fields with a consistent extent can establish it.
            if !plain {
                return None;
            }
            let extent = count.or_else(|| value.map(|value| value.encode_utf16().count()))?;
            if end > extent || value.is_some_and(|value| value.encode_utf16().count() != extent) {
                return None;
            }
            return Some((String::new(), "selection at field start"));
        }
        if let Some(prefix) = range_text(field, 0, location) {
            return Some((prefix, "text range"));
        }
        // AXValue offsets are not interchangeable with rich-document ranges.
        if plain {
            let units: Vec<_> = value?.encode_utf16().collect();
            if end <= units.len() && count.is_none_or(|count| count == units.len()) {
                return String::from_utf16(&units[..location])
                    .ok()
                    .map(|prefix| (prefix, "text value"));
            }
        }
        None
    }

    unsafe fn editable_ancestor(start: Ref) -> Option<(Owned, String)> {
        let mut element = Owned::retained(start);
        for _ in 0..8 {
            let role = text(element.0, c"AXRole")?;
            if text(element.0, c"AXSubrole").as_deref() == Some("AXSecureTextField") {
                return None;
            }
            if editable(&role) {
                return Some((element, role));
            }
            if matches!(role.as_str(), "AXWebArea" | "AXWindow" | "AXApplication") {
                return None;
            }
            element = attribute(element.0, c"AXParent")?;
        }
        None
    }
    unsafe fn secure_ancestor(start: Ref) -> bool {
        let mut element = Owned::retained(start);
        for _ in 0..8 {
            if text(element.0, c"AXSubrole").as_deref() == Some("AXSecureTextField") {
                return true;
            }
            if matches!(
                text(element.0, c"AXRole").as_deref(),
                Some("AXWebArea" | "AXWindow" | "AXApplication")
            ) {
                break;
            }
            let Some(parent) = attribute(element.0, c"AXParent") else {
                break;
            };
            element = parent;
        }
        false
    }
    unsafe fn is_descendant(element: Ref, root: Ref) -> bool {
        let mut element = Owned::retained(element);
        for _ in 0..16 {
            if CFEqual(element.0, root) {
                return true;
            }
            let Some(parent) = attribute(element.0, c"AXParent") else {
                return false;
            };
            element = parent;
        }
        false
    }
    unsafe fn marker_range(value: Owned) -> Option<Owned> {
        (CFGetTypeID(value.0) == AXTextMarkerRangeGetTypeID()).then_some(value)
    }

    unsafe fn ordered_markers(owner: Ref, first: Ref, second: Ref) -> Option<Owned> {
        // CFType callbacks retain both markers for the complete synchronous AX call.
        // AXIndexForTextMarker cannot order markers from different text nodes.
        let markers = [first, second];
        let arguments = Owned::new(CFArrayCreate(
            std::ptr::null(),
            markers.as_ptr(),
            markers.len() as isize,
            &kCFTypeArrayCallBacks,
        ))?;
        marker_range(parameter(
            owner,
            c"AXTextMarkerRangeForUnorderedTextMarkers",
            arguments.0,
        )?)
    }

    unsafe fn marker_element(owner: Ref, marker: Ref, field: Ref) -> Option<Owned> {
        let element = parameter(owner, c"AXUIElementForTextMarker", marker)?;
        is_descendant(element.0, field).then_some(element)
    }

    unsafe fn text_between(owner: Ref, start: Ref, end: Ref) -> Option<String> {
        let range = Owned::new(AXTextMarkerRangeCreate(std::ptr::null(), start, end))?;
        string(parameter(owner, c"AXStringForTextMarkerRange", range.0)?.0)
    }

    unsafe fn scoped_marker_prefix(owner: Ref, scope: Ref, caret: Ref) -> Option<(String, String)> {
        marker_element(owner, caret, scope)?;
        let range = marker_range(parameter(owner, c"AXTextMarkerRangeForUIElement", scope)?)?;
        let start = Owned::new(AXTextMarkerRangeCopyStartMarker(range.0))?;
        let end = Owned::new(AXTextMarkerRangeCopyEndMarker(range.0))?;
        // Some older implementations ignore the UI-element parameter. Reject a
        // document range returned in place of this editor/paragraph's range.
        marker_element(owner, start.0, scope)?;
        marker_element(owner, end.0, scope)?;
        let prefix = text_between(owner, start.0, caret)?;
        let full = string(parameter(owner, c"AXStringForTextMarkerRange", range.0)?.0)?;
        full.starts_with(&prefix).then_some((prefix, full))
    }

    unsafe fn paragraph_prefix(owner: Ref, field: Ref, caret: Ref) -> Option<String> {
        let mut element = marker_element(owner, caret, field)?;
        for _ in 0..16 {
            if CFEqual(element.0, field) {
                break;
            }
            let role = text(element.0, c"AXRole");
            let subrole = text(element.0, c"AXSubrole");
            if role.as_deref() == Some("AXParagraph") || subrole.as_deref() == Some("AXParagraph") {
                return scoped_marker_prefix(owner, element.0, caret).map(|(prefix, _)| prefix);
            }
            element = attribute(element.0, c"AXParent")?;
        }

        // Chromium exposes paragraph containers as AXGroup and omits generated
        // paragraph newlines from marker strings. Its paragraph-range API can
        // also clip to an inline text anchor. Verify the candidate start using a
        // cross-anchor paragraph traversal before trusting an empty prefix.
        let paragraph = marker_range(parameter(
            owner,
            c"AXParagraphTextMarkerRangeForTextMarker",
            caret,
        )?)?;
        let start = Owned::new(AXTextMarkerRangeCopyStartMarker(paragraph.0))?;
        marker_element(owner, start.0, field)?;
        let next = parameter(owner, c"AXNextTextMarkerForTextMarker", start.0)?;
        marker_element(owner, next.0, field)?;
        let verified_start = parameter(
            owner,
            c"AXPreviousParagraphStartTextMarkerForTextMarker",
            next.0,
        )?;
        marker_element(owner, verified_start.0, field)?;
        if !CFEqual(start.0, verified_start.0) {
            return None;
        }
        text_between(owner, start.0, caret)
    }

    struct MarkerContext {
        prefix: String,
        location: usize,
        selected: usize,
        length: usize,
        paragraph: bool,
    }

    /// Scope opaque markers to the editor instead of AXStartTextMarker, which
    /// can refer to the document. Never compare offsets from different anchors.
    unsafe fn marker_prefix(owner: Ref, field: Ref) -> Option<MarkerContext> {
        let selected = marker_range(attribute(owner, c"AXSelectedTextMarkerRange")?)?;
        let start = Owned::new(AXTextMarkerRangeCopyStartMarker(selected.0))?;
        let end = Owned::new(AXTextMarkerRangeCopyEndMarker(selected.0))?;
        marker_element(owner, start.0, field)?;
        marker_element(owner, end.0, field)?;
        let ordered = if CFEqual(start.0, end.0) {
            selected
        } else {
            ordered_markers(owner, start.0, end.0)?
        };
        let caret = Owned::new(AXTextMarkerRangeCopyStartMarker(ordered.0))?;
        let (prefix, full) = scoped_marker_prefix(owner, field, caret.0)?;
        let selected_text = string(parameter(owner, c"AXStringForTextMarkerRange", ordered.0)?.0)?;
        let location = prefix.encode_utf16().count();
        let selected = selected_text.encode_utf16().count();
        let length = full.encode_utf16().count();
        if location.checked_add(selected)? > length {
            return None;
        }
        let paragraph = paragraph_prefix(owner, field, caret.0);
        Some(MarkerContext {
            prefix: paragraph.as_ref().unwrap_or(&prefix).clone(),
            location,
            selected,
            length,
            paragraph: paragraph.is_some(),
        })
    }

    pub(super) fn capture() -> Snapshot {
        let _guard = BudgetGuard::start();
        let mut result = capture_with_budget();
        if result.boundary.is_none() {
            CAPTURE_BUDGET.with(|budget| {
                if let Some(budget) = budget.get() {
                    if budget.cannot_complete {
                        result.status = "editor accessibility request could not complete".into();
                    } else if budget.request_timeout(Instant::now()).is_none() {
                        result.status = "editor context capture deadline reached".into();
                    }
                }
            });
        }
        result
    }

    fn capture_with_budget() -> Snapshot {
        let mut result = Snapshot::default();
        unsafe {
            if !AXIsProcessTrusted() {
                result.status = "Accessibility permission unavailable".into();
                return result;
            }
            result.status = "focused application unavailable".into();
            let Some(system) = Owned::new(AXUIElementCreateSystemWide()) else {
                return result;
            };
            let Some(application) = attribute(system.0, c"AXFocusedApplication") else {
                return result;
            };
            let Some(pid) = application_pid(application.0) else {
                return result;
            };
            // Backup for previews and very short dictations. Preparation at
            // recording start normally allows Electron's debounce to finish.
            prepare_application(application.0, pid);
            result.status = "focused editable element unavailable".into();
            let Some(focused) = attribute(application.0, c"AXFocusedUIElement") else {
                return result;
            };
            if secure_ancestor(focused.0) {
                result.status = "secure field excluded".into();
                return result;
            }
            let window = attribute(focused.0, c"AXWindow")
                .or_else(|| attribute(application.0, c"AXFocusedWindow"));
            // This is only an opaque continuation target, never evidence that a
            // generic container is empty or that its default zero range is valid.
            // Retain the opaque focus identity rather than sharing continuation
            // state across every inaccessible field in this window. Without a
            // window identity, do not create a continuation target at all.
            result.target = window.as_ref().map(|window| Target {
                pid,
                window: CFHash(window.0),
                field: CFHash(focused.0),
            });
            let resolved = editable_ancestor(focused.0).or_else(|| {
                let selected = marker_range(attribute(focused.0, c"AXSelectedTextMarkerRange")?)?;
                let caret = Owned::new(AXTextMarkerRangeCopyStartMarker(selected.0))?;
                let element = parameter(focused.0, c"AXUIElementForTextMarker", caret.0)?;
                editable_ancestor(element.0)
            });
            let Some((field, role)) = resolved else {
                result.role = text(focused.0, c"AXRole").unwrap_or_default();
                return result;
            };
            result.role = role;
            let field_window = attribute(field.0, c"AXWindow")
                .or_else(|| attribute(application.0, c"AXFocusedWindow"))
                .or(window);
            result.target = field_window.map(|window| Target {
                pid,
                window: CFHash(window.0),
                field: CFHash(field.0),
            });
            result.selection = selection(field.0);
            result.status = "editor does not expose a usable selection and text range".into();

            let value = text(field.0, c"AXValue");
            let count =
                attribute(field.0, c"AXNumberOfCharacters").and_then(|value| number(value.0));
            let children = child_count(field.0);
            // Native text views and atomic browser inputs have no text subtree.
            // Rich contenteditables expose their nested paragraphs/inline nodes.
            let plain = children == Some(0) || (children.is_none() && result.role == "AXTextField");
            result.text_length = count.or_else(|| {
                plain
                    .then(|| value.as_ref().map(|text| text.encode_utf16().count()))
                    .flatten()
            });
            if plain {
                if let Some((prefix, source)) = result.selection.and_then(|selection| {
                    ordinary_prefix(field.0, selection, count, value.as_deref(), true)
                }) {
                    result.boundary = Some(capitalize_prefix(&prefix));
                    result.source = source;
                    result.status = "available".into();
                    return result;
                }
                if result.selection.is_none()
                    && children == Some(0)
                    && count == Some(0)
                    && value.as_ref().is_some_and(|value| value.is_empty())
                {
                    result.boundary = Some(true);
                    result.source = "confirmed empty field";
                    result.status = "available".into();
                    return result;
                }
            }

            // Marker ordering and containment come from the editor's own opaque
            // ranges. Rich paragraph context is read separately because Chromium
            // can omit generated separators from a root's plain marker string.
            let mut owner = Owned::retained(field.0);
            for _ in 0..6 {
                if let Some(context) = marker_prefix(owner.0, field.0) {
                    result.boundary = Some(capitalize_prefix(&context.prefix));
                    result.selection = Some((context.location, context.selected));
                    result.text_length = Some(context.length);
                    result.source = if context.paragraph {
                        "scoped paragraph markers"
                    } else {
                        "scoped text markers"
                    };
                    result.status = "available".into();
                    return result;
                }
                if text(owner.0, c"AXRole").as_deref() == Some("AXWebArea") {
                    break;
                }
                let Some(parent) = attribute(owner.0, c"AXParent") else {
                    break;
                };
                if matches!(
                    text(parent.0, c"AXRole").as_deref(),
                    Some("AXWindow" | "AXApplication")
                ) {
                    break;
                }
                owner = parent;
            }

            if let Some((prefix, source)) = result.selection.and_then(|selection| {
                ordinary_prefix(field.0, selection, count, value.as_deref(), plain)
            }) {
                result.boundary = Some(capitalize_prefix(&prefix));
                result.source = source;
            }
            if result.boundary.is_some() {
                result.status = "available".into();
            }
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn absent_ax_values_never_create_an_owned_null_reference() {
            // Regression: then_some(Owned(null)) eagerly constructed and dropped
            // an owner, causing CFRelease(null) on unsupported AX attributes.
            assert!(unsafe { Owned::new(std::ptr::null()) }.is_none());
        }

        #[test]
        fn request_timeouts_shrink_and_expired_budget_does_not_restart() {
            let now = Instant::now();
            let budget = CaptureBudget {
                deadline: now + CAPTURE_TIMEOUT,
                cannot_complete: false,
            };
            assert_eq!(budget.request_timeout(now), Some(REQUEST_TIMEOUT));
            assert_eq!(
                budget.request_timeout(now + Duration::from_millis(550)),
                Some(Duration::from_millis(50))
            );
            assert_eq!(budget.request_timeout(now + CAPTURE_TIMEOUT), None);
            assert_eq!(
                budget.request_timeout(now + CAPTURE_TIMEOUT + Duration::from_secs(1)),
                None
            );
        }

        #[test]
        fn cannot_complete_stops_later_ax_requests_in_the_same_capture() {
            let _guard = BudgetGuard::start();
            record_status(AX_CANNOT_COMPLETE);
            // These deliberately invalid references must never reach AX after a
            // stalled request. Both call paths stop at the shared budget check.
            unsafe {
                assert!(attribute(std::ptr::null(), c"AXValue").is_none());
                assert!(
                    parameter(std::ptr::null(), c"AXStringForRange", std::ptr::null()).is_none()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn unknown(pid: i32, window: usize, field: usize) -> Snapshot {
        Snapshot {
            target: Some(Target { pid, window, field }),
            ..Snapshot::default()
        }
    }
    #[test]
    fn boundaries_are_distinct_from_spaces_in_unfinished_sentences() {
        for prefix in ["", "   ", "hello.  ", "hello? ", "hello!\" ", "hello\n  "] {
            assert!(capitalize_prefix(prefix), "{prefix:?}");
        }
        for prefix in ["hello", "hello  ", "hello\nworld", "😀hello "] {
            assert!(!capitalize_prefix(prefix), "{prefix:?}");
        }
    }
    #[test]
    fn only_committed_insertions_change_per_editor_continuation() {
        let now = Instant::now();
        let mut state = Continuations::default();
        let a = unknown(1, 10, 100);
        let b = unknown(2, 20, 200);
        assert_eq!(state.lookup(&a, now), None);
        state.commit(&a, "Hello ", now);
        assert_eq!(state.lookup(&a, now), Some(false));
        state.commit(&b, "Other? ", now);
        assert_eq!(state.lookup(&a, now), Some(false));
        assert_eq!(state.lookup(&b, now), Some(true));
        state.commit(&a, "\n  ", now);
        assert_eq!(state.lookup(&a, now), Some(true));
        assert_eq!(state.lookup(&unknown(1, 10, 101), now), None);
    }
    #[test]
    fn changed_caret_or_different_field_cannot_inherit_a_continuation() {
        let now = Instant::now();
        let mut state = Continuations::default();
        let mut before = unknown(1, 10, 100);
        before.selection = Some((3, 2));
        state.commit(&before, "😀hello ", now);
        let mut after = unknown(1, 10, 100);
        after.selection = Some((11, 0));
        assert_eq!(state.lookup(&after, now), Some(false));
        let mut other_field = unknown(1, 10, 101);
        other_field.selection = after.selection;
        assert_eq!(state.lookup(&other_field, now), None);
        after.selection = Some((0, 0));
        assert_eq!(state.lookup(&after, now), None);
        assert_eq!(state.lookup(&before, now + Duration::from_secs(901)), None);
    }
}
