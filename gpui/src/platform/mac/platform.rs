use super::{BoolExt as _, Dispatcher, FontSystem, Window};
use crate::{executor, keymap::Keystroke, platform, ClipboardItem, Event, Menu, MenuItem};
use block::ConcreteBlock;
use cocoa::{
    appkit::{
        NSApplication, NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular,
        NSEventModifierFlags, NSMenu, NSMenuItem, NSModalResponse, NSOpenPanel, NSPasteboard,
        NSPasteboardTypeString, NSSavePanel, NSWindow,
    },
    base::{id, nil, selector},
    foundation::{NSArray, NSAutoreleasePool, NSData, NSInteger, NSString, NSURL},
};
use ctor::ctor;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use ptr::null_mut;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    convert::TryInto,
    ffi::{c_void, CStr},
    os::raw::c_char,
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    slice, str,
    sync::Arc,
};

const MAC_PLATFORM_IVAR: &'static str = "platform";
static mut APP_CLASS: *const Class = ptr::null();
static mut APP_DELEGATE_CLASS: *const Class = ptr::null();

#[ctor]
unsafe fn build_classes() {
    APP_CLASS = {
        let mut decl = ClassDecl::new("GPUIApplication", class!(NSApplication)).unwrap();
        decl.add_ivar::<*mut c_void>(MAC_PLATFORM_IVAR);
        decl.add_method(
            sel!(sendEvent:),
            send_event as extern "C" fn(&mut Object, Sel, id),
        );
        decl.register()
    };

    APP_DELEGATE_CLASS = {
        let mut decl = ClassDecl::new("GPUIApplicationDelegate", class!(NSResponder)).unwrap();
        decl.add_ivar::<*mut c_void>(MAC_PLATFORM_IVAR);
        decl.add_method(
            sel!(applicationDidFinishLaunching:),
            did_finish_launching as extern "C" fn(&mut Object, Sel, id),
        );
        decl.add_method(
            sel!(applicationDidBecomeActive:),
            did_become_active as extern "C" fn(&mut Object, Sel, id),
        );
        decl.add_method(
            sel!(applicationDidResignActive:),
            did_resign_active as extern "C" fn(&mut Object, Sel, id),
        );
        decl.add_method(
            sel!(handleGPUIMenuItem:),
            handle_menu_item as extern "C" fn(&mut Object, Sel, id),
        );
        decl.add_method(
            sel!(application:openFiles:),
            open_files as extern "C" fn(&mut Object, Sel, id, id),
        );
        decl.register()
    }
}

#[derive(Default)]
pub struct MacForegroundPlatform(RefCell<MacForegroundPlatformState>);

#[derive(Default)]
pub struct MacForegroundPlatformState {
    become_active: Option<Box<dyn FnMut()>>,
    resign_active: Option<Box<dyn FnMut()>>,
    event: Option<Box<dyn FnMut(crate::Event) -> bool>>,
    menu_command: Option<Box<dyn FnMut(&str, Option<&dyn Any>)>>,
    open_files: Option<Box<dyn FnMut(Vec<PathBuf>)>>,
    finish_launching: Option<Box<dyn FnOnce() -> ()>>,
    menu_actions: Vec<(String, Option<Box<dyn Any>>)>,
}

impl MacForegroundPlatform {
    unsafe fn create_menu_bar(&self, menus: Vec<Menu>) -> id {
        let menu_bar = NSMenu::new(nil).autorelease();
        let mut state = self.0.borrow_mut();

        state.menu_actions.clear();

        for menu_config in menus {
            let menu_bar_item = NSMenuItem::new(nil).autorelease();
            let menu = NSMenu::new(nil).autorelease();
            let menu_name = menu_config.name;

            menu.setTitle_(ns_string(menu_name));

            for item_config in menu_config.items {
                let item;

                match item_config {
                    MenuItem::Separator => {
                        item = NSMenuItem::separatorItem(nil);
                    }
                    MenuItem::Action {
                        name,
                        keystroke,
                        action,
                        arg,
                    } => {
                        if let Some(keystroke) = keystroke {
                            let keystroke = Keystroke::parse(keystroke).unwrap_or_else(|err| {
                                panic!(
                                    "Invalid keystroke for menu item {}:{} - {:?}",
                                    menu_name, name, err
                                )
                            });

                            let mut mask = NSEventModifierFlags::empty();
                            for (modifier, flag) in &[
                                (keystroke.cmd, NSEventModifierFlags::NSCommandKeyMask),
                                (keystroke.ctrl, NSEventModifierFlags::NSControlKeyMask),
                                (keystroke.alt, NSEventModifierFlags::NSAlternateKeyMask),
                            ] {
                                if *modifier {
                                    mask |= *flag;
                                }
                            }

                            item = NSMenuItem::alloc(nil)
                                .initWithTitle_action_keyEquivalent_(
                                    ns_string(name),
                                    selector("handleGPUIMenuItem:"),
                                    ns_string(&keystroke.key),
                                )
                                .autorelease();
                            item.setKeyEquivalentModifierMask_(mask);
                        } else {
                            item = NSMenuItem::alloc(nil)
                                .initWithTitle_action_keyEquivalent_(
                                    ns_string(name),
                                    selector("handleGPUIMenuItem:"),
                                    ns_string(""),
                                )
                                .autorelease();
                        }

                        let tag = state.menu_actions.len() as NSInteger;
                        let _: () = msg_send![item, setTag: tag];
                        state.menu_actions.push((action.to_string(), arg));
                    }
                }

                menu.addItem_(item);
            }

            menu_bar_item.setSubmenu_(menu);
            menu_bar.addItem_(menu_bar_item);
        }

        menu_bar
    }
}

impl platform::ForegroundPlatform for MacForegroundPlatform {
    fn on_become_active(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().become_active = Some(callback);
    }

    fn on_resign_active(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().resign_active = Some(callback);
    }

    fn on_event(&self, callback: Box<dyn FnMut(crate::Event) -> bool>) {
        self.0.borrow_mut().event = Some(callback);
    }

    fn on_open_files(&self, callback: Box<dyn FnMut(Vec<PathBuf>)>) {
        self.0.borrow_mut().open_files = Some(callback);
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce() -> ()>) {
        self.0.borrow_mut().finish_launching = Some(on_finish_launching);

        unsafe {
            let app: id = msg_send![APP_CLASS, sharedApplication];
            let app_delegate: id = msg_send![APP_DELEGATE_CLASS, new];
            app.setDelegate_(app_delegate);

            let self_ptr = self as *const Self as *const c_void;
            (*app).set_ivar(MAC_PLATFORM_IVAR, self_ptr);
            (*app_delegate).set_ivar(MAC_PLATFORM_IVAR, self_ptr);

            let pool = NSAutoreleasePool::new(nil);
            app.run();
            pool.drain();

            (*app).set_ivar(MAC_PLATFORM_IVAR, null_mut::<c_void>());
            (*app.delegate()).set_ivar(MAC_PLATFORM_IVAR, null_mut::<c_void>());
        }
    }

    fn on_menu_command(&self, callback: Box<dyn FnMut(&str, Option<&dyn Any>)>) {
        self.0.borrow_mut().menu_command = Some(callback);
    }

    fn set_menus(&self, menus: Vec<Menu>) {
        unsafe {
            let app: id = msg_send![APP_CLASS, sharedApplication];
            app.setMainMenu_(self.create_menu_bar(menus));
        }
    }

    fn prompt_for_paths(
        &self,
        options: platform::PathPromptOptions,
        done_fn: Box<dyn FnOnce(Option<Vec<std::path::PathBuf>>)>,
    ) {
        unsafe {
            let panel = NSOpenPanel::openPanel(nil);
            panel.setCanChooseDirectories_(options.directories.to_objc());
            panel.setCanChooseFiles_(options.files.to_objc());
            panel.setAllowsMultipleSelection_(options.multiple.to_objc());
            panel.setResolvesAliases_(false.to_objc());
            let done_fn = Cell::new(Some(done_fn));
            let block = ConcreteBlock::new(move |response: NSModalResponse| {
                let result = if response == NSModalResponse::NSModalResponseOk {
                    let mut result = Vec::new();
                    let urls = panel.URLs();
                    for i in 0..urls.count() {
                        let url = urls.objectAtIndex(i);
                        let string = url.absoluteString();
                        let string = std::ffi::CStr::from_ptr(string.UTF8String())
                            .to_string_lossy()
                            .to_string();
                        if let Some(path) = string.strip_prefix("file://") {
                            result.push(PathBuf::from(path));
                        }
                    }
                    Some(result)
                } else {
                    None
                };

                if let Some(done_fn) = done_fn.take() {
                    (done_fn)(result);
                }
            });
            let block = block.copy();
            let _: () = msg_send![panel, beginWithCompletionHandler: block];
        }
    }

    fn prompt_for_new_path(
        &self,
        directory: &Path,
        done_fn: Box<dyn FnOnce(Option<std::path::PathBuf>)>,
    ) {
        unsafe {
            let panel = NSSavePanel::savePanel(nil);
            let path = ns_string(directory.to_string_lossy().as_ref());
            let url = NSURL::fileURLWithPath_isDirectory_(nil, path, true.to_objc());
            panel.setDirectoryURL(url);

            let done_fn = Cell::new(Some(done_fn));
            let block = ConcreteBlock::new(move |response: NSModalResponse| {
                let result = if response == NSModalResponse::NSModalResponseOk {
                    let url = panel.URL();
                    let string = url.absoluteString();
                    let string = std::ffi::CStr::from_ptr(string.UTF8String())
                        .to_string_lossy()
                        .to_string();
                    if let Some(path) = string.strip_prefix("file://") {
                        Some(PathBuf::from(path))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(done_fn) = done_fn.take() {
                    (done_fn)(result);
                }
            });
            let block = block.copy();
            let _: () = msg_send![panel, beginWithCompletionHandler: block];
        }
    }
}

pub struct MacPlatform {
    dispatcher: Arc<Dispatcher>,
    fonts: Arc<FontSystem>,
    pasteboard: id,
    text_hash_pasteboard_type: id,
    metadata_pasteboard_type: id,
}

impl MacPlatform {
    pub fn new() -> Self {
        Self {
            dispatcher: Arc::new(Dispatcher),
            fonts: Arc::new(FontSystem::new()),
            pasteboard: unsafe { NSPasteboard::generalPasteboard(nil) },
            text_hash_pasteboard_type: unsafe { ns_string("zed-text-hash") },
            metadata_pasteboard_type: unsafe { ns_string("zed-metadata") },
        }
    }

    unsafe fn read_from_pasteboard(&self, kind: id) -> Option<&[u8]> {
        let data = self.pasteboard.dataForType(kind);
        if data == nil {
            None
        } else {
            Some(slice::from_raw_parts(
                data.bytes() as *mut u8,
                data.length() as usize,
            ))
        }
    }
}

unsafe impl Send for MacPlatform {}
unsafe impl Sync for MacPlatform {}

impl platform::Platform for MacPlatform {
    fn dispatcher(&self) -> Arc<dyn platform::Dispatcher> {
        self.dispatcher.clone()
    }

    fn activate(&self, ignoring_other_apps: bool) {
        unsafe {
            let app = NSApplication::sharedApplication(nil);
            app.activateIgnoringOtherApps_(ignoring_other_apps.to_objc());
        }
    }

    fn open_window(
        &self,
        id: usize,
        options: platform::WindowOptions,
        executor: Rc<executor::Foreground>,
    ) -> Box<dyn platform::Window> {
        Box::new(Window::open(id, options, executor, self.fonts()))
    }

    fn key_window_id(&self) -> Option<usize> {
        Window::key_window_id()
    }

    fn fonts(&self) -> Arc<dyn platform::FontSystem> {
        self.fonts.clone()
    }

    fn quit(&self) {
        // Quitting the app causes us to close windows, which invokes `Window::on_close` callbacks
        // synchronously before this method terminates. If we call `Platform::quit` while holding a
        // borrow of the app state (which most of the time we will do), we will end up
        // double-borrowing the app state in the `on_close` callbacks for our open windows. To solve
        // this, we make quitting the application asynchronous so that we aren't holding borrows to
        // the app state on the stack when we actually terminate the app.

        use super::dispatcher::{dispatch_async_f, dispatch_get_main_queue};

        unsafe {
            dispatch_async_f(dispatch_get_main_queue(), ptr::null_mut(), Some(quit));
        }

        unsafe extern "C" fn quit(_: *mut c_void) {
            let app = NSApplication::sharedApplication(nil);
            let _: () = msg_send![app, terminate: nil];
        }
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        unsafe {
            self.pasteboard.clearContents();

            let text_bytes = NSData::dataWithBytes_length_(
                nil,
                item.text.as_ptr() as *const c_void,
                item.text.len() as u64,
            );
            self.pasteboard
                .setData_forType(text_bytes, NSPasteboardTypeString);

            if let Some(metadata) = item.metadata.as_ref() {
                let hash_bytes = ClipboardItem::text_hash(&item.text).to_be_bytes();
                let hash_bytes = NSData::dataWithBytes_length_(
                    nil,
                    hash_bytes.as_ptr() as *const c_void,
                    hash_bytes.len() as u64,
                );
                self.pasteboard
                    .setData_forType(hash_bytes, self.text_hash_pasteboard_type);

                let metadata_bytes = NSData::dataWithBytes_length_(
                    nil,
                    metadata.as_ptr() as *const c_void,
                    metadata.len() as u64,
                );
                self.pasteboard
                    .setData_forType(metadata_bytes, self.metadata_pasteboard_type);
            }
        }
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        unsafe {
            if let Some(text_bytes) = self.read_from_pasteboard(NSPasteboardTypeString) {
                let text = String::from_utf8_lossy(&text_bytes).to_string();
                let hash_bytes = self
                    .read_from_pasteboard(self.text_hash_pasteboard_type)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u64::from_be_bytes);
                let metadata_bytes = self
                    .read_from_pasteboard(self.metadata_pasteboard_type)
                    .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok());

                if let Some((hash, metadata)) = hash_bytes.zip(metadata_bytes) {
                    if hash == ClipboardItem::text_hash(&text) {
                        Some(ClipboardItem {
                            text,
                            metadata: Some(metadata),
                        })
                    } else {
                        Some(ClipboardItem {
                            text,
                            metadata: None,
                        })
                    }
                } else {
                    Some(ClipboardItem {
                        text,
                        metadata: None,
                    })
                }
            } else {
                None
            }
        }
    }
}

unsafe fn get_foreground_platform(object: &mut Object) -> &MacForegroundPlatform {
    let platform_ptr: *mut c_void = *object.get_ivar(MAC_PLATFORM_IVAR);
    assert!(!platform_ptr.is_null());
    &*(platform_ptr as *const MacForegroundPlatform)
}

extern "C" fn send_event(this: &mut Object, _sel: Sel, native_event: id) {
    unsafe {
        if let Some(event) = Event::from_native(native_event, None) {
            let platform = get_foreground_platform(this);
            if let Some(callback) = platform.0.borrow_mut().event.as_mut() {
                if callback(event) {
                    return;
                }
            }
        }

        msg_send![super(this, class!(NSApplication)), sendEvent: native_event]
    }
}

extern "C" fn did_finish_launching(this: &mut Object, _: Sel, _: id) {
    unsafe {
        let app: id = msg_send![APP_CLASS, sharedApplication];
        app.setActivationPolicy_(NSApplicationActivationPolicyRegular);

        let platform = get_foreground_platform(this);
        let callback = platform.0.borrow_mut().finish_launching.take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

extern "C" fn did_become_active(this: &mut Object, _: Sel, _: id) {
    let platform = unsafe { get_foreground_platform(this) };
    if let Some(callback) = platform.0.borrow_mut().become_active.as_mut() {
        callback();
    }
}

extern "C" fn did_resign_active(this: &mut Object, _: Sel, _: id) {
    let platform = unsafe { get_foreground_platform(this) };
    if let Some(callback) = platform.0.borrow_mut().resign_active.as_mut() {
        callback();
    }
}

extern "C" fn open_files(this: &mut Object, _: Sel, _: id, paths: id) {
    let paths = unsafe {
        (0..paths.count())
            .into_iter()
            .filter_map(|i| {
                let path = paths.objectAtIndex(i);
                match CStr::from_ptr(path.UTF8String() as *mut c_char).to_str() {
                    Ok(string) => Some(PathBuf::from(string)),
                    Err(err) => {
                        log::error!("error converting path to string: {}", err);
                        None
                    }
                }
            })
            .collect::<Vec<_>>()
    };
    let platform = unsafe { get_foreground_platform(this) };
    if let Some(callback) = platform.0.borrow_mut().open_files.as_mut() {
        callback(paths);
    }
}

extern "C" fn handle_menu_item(this: &mut Object, _: Sel, item: id) {
    unsafe {
        let platform = get_foreground_platform(this);
        let mut platform = platform.0.borrow_mut();
        if let Some(mut callback) = platform.menu_command.take() {
            let tag: NSInteger = msg_send![item, tag];
            let index = tag as usize;
            if let Some((action, arg)) = platform.menu_actions.get(index) {
                callback(action, arg.as_ref().map(Box::as_ref));
            }
            platform.menu_command = Some(callback);
        }
    }
}

unsafe fn ns_string(string: &str) -> id {
    NSString::alloc(nil).init_str(string).autorelease()
}

#[cfg(test)]
mod tests {
    use crate::platform::Platform;

    use super::*;

    #[test]
    fn test_clipboard() {
        let platform = build_platform();
        assert_eq!(platform.read_from_clipboard(), None);

        let item = ClipboardItem::new("1".to_string());
        platform.write_to_clipboard(item.clone());
        assert_eq!(platform.read_from_clipboard(), Some(item));

        let item = ClipboardItem::new("2".to_string()).with_metadata(vec![3, 4]);
        platform.write_to_clipboard(item.clone());
        assert_eq!(platform.read_from_clipboard(), Some(item));

        let text_from_other_app = "text from other app";
        unsafe {
            let bytes = NSData::dataWithBytes_length_(
                nil,
                text_from_other_app.as_ptr() as *const c_void,
                text_from_other_app.len() as u64,
            );
            platform
                .pasteboard
                .setData_forType(bytes, NSPasteboardTypeString);
        }
        assert_eq!(
            platform.read_from_clipboard(),
            Some(ClipboardItem::new(text_from_other_app.to_string()))
        );
    }

    fn build_platform() -> MacPlatform {
        let mut platform = MacPlatform::new();
        platform.pasteboard = unsafe { NSPasteboard::pasteboardWithUniqueName(nil) };
        platform
    }
}
