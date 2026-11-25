use std::{cell::{Cell, RefCell}, collections::BTreeSet, fs::{self, File, OpenOptions}, io::{Read, Write}, path::{Path, PathBuf}, rc::Rc, time::{Duration, Instant}};
use serde::{Deserialize, Serialize};
use tauri::{Manager, RunEvent, WindowEvent};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, BrowserProcessExitedEventHandler, CoTaskMemPWSTR,
    ProcessInfosChangedEventHandler,
};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use windows::{
    core::{Interface, IUnknown, PWSTR},
    Win32::{
        Foundation::HWND,
        UI::{
            Input::KeyboardAndMouse::{
                GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::{KillTimer, SetTimer},
        },
    },
};
use crate::{connection::{Bridge, RuntimeInput}, intent::ordinary};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopEnrollment {
    version: u32,
    webview_version: String,
    runtime: RuntimeInput,
}

pub struct Launch {
    pub client: PathBuf,
    pub profile: PathBuf,
    pub runtime: RuntimeInput,
    webview_version: String,
    native_log: PathBuf,
}

impl Launch {
    pub fn from_app_data(app_data: &Path) -> Result<Self, String> {
        const MAX_ENROLLMENT_BYTES: usize = 8192;
        let path = app_data.join("enrollment.json");
        ordinary(&path, false)?;
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| file.take(MAX_ENROLLMENT_BYTES as u64 + 1).read_to_end(&mut bytes))
            .map_err(|_| "Cannot read desktop enrollment.")?;
        if bytes.len() > MAX_ENROLLMENT_BYTES {
            return Err("Desktop enrollment exceeds bound.".into());
        }

        let DesktopEnrollment { version, webview_version, runtime } =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid desktop enrollment.")?;
        if version != 1
            || webview_version.is_empty()
            || webview_version.len() > 128
            || !webview_version.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return Err("Invalid desktop enrollment version or WebView2 pin.".into());
        }

        ordinary(&runtime.control, true)?;
        ordinary(&runtime.enrollment.peer_image, false)?;
        crate::intent::id(&runtime.enrollment.instance)?;
        let peer_hash = &runtime.enrollment.peer_sha256;
        if peer_hash.len() != 64
            || !peer_hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Invalid runtime image pin.".into());
        }

        let instance_root = app_data.join("instances").join(&runtime.enrollment.instance);
        let client = instance_root.join("client");
        let profile = instance_root.join("profile");
        let evidence = instance_root.join("evidence");
        for directory in [&client, &profile, &evidence] {
            ordinary(directory, true)?;
        }

        let launch_id = yokai_platform::random_instance_id()
            .map_err(|_| "Cannot allocate a desktop launch identity.")?;
        let native_log = evidence.join(format!("desktop-native-{launch_id}.jsonl"));
        Ok(Self { client, profile, runtime, webview_version, native_log })
    }
}

thread_local! {
    static LIFE: RefCell<Option<Life>> = const { RefCell::new(None) };
    static CALLBACK_FAULT: Cell<bool> = const { Cell::new(false) };
    static EXIT_READY: Cell<bool> = const { Cell::new(false) };
}

struct Events {
    environment: usize,
    browser: u32,
    changed: Cell<bool>,
    exit: Cell<Option<(u32, i32)>>,
    fault: Cell<bool>,
    shortcut: Cell<Option<Shortcut>>,
}
#[derive(Serialize)]
struct ProcessFact { pid: i32, kind: i32 }

struct Life {
    app: tauri::AppHandle,
    window: Option<tauri::WebviewWindow>,
    controller: Option<ICoreWebView2Controller>,
    env5: ICoreWebView2Environment5,
    env8: ICoreWebView2Environment8,
    flags: Rc<Events>,
    browser_token: Option<i64>,
    process_token: Option<i64>,
    accelerator_token: Option<i64>,
    timer: usize,
    expected_profile: PathBuf,
    expected_version: String,
    log: File,
    log_bytes: usize,
    started: Instant,
    sampled: Instant,
    closing: Option<Instant>,
    destroy_requested: bool,
    destroyed: bool,
    failed: bool,
}

fn com<T>(result: windows::core::Result<T>) -> Result<T,String> { result.map_err(|e| format!("Native WebView API failed: {}", e.code())) }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shortcut {
    Close,
    Quit,
}

fn window_shortcut(
    kind: COREWEBVIEW2_KEY_EVENT_KIND,
    key: u32,
    control_down: bool,
    other_modifier_down: bool,
) -> Option<Shortcut> {
    if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN || !control_down || other_modifier_down {
        return None;
    }
    match u8::try_from(key).ok()? {
        b'W' => Some(Shortcut::Close),
        b'Q' => Some(Shortcut::Quit),
        _ => None,
    }
}

fn key_is_down(key: VIRTUAL_KEY) -> bool {
    (unsafe { GetKeyState(i32::from(key.0)) }) < 0
}

fn strict_string(pointer: PWSTR, maximum: usize) -> Result<String,String> {
    let _owner = CoTaskMemPWSTR::from(pointer);
    if pointer.is_null() { return Err("Missing WebView string.".into()); }
    // The SDK returns a valid NUL-terminated allocation; do not use lossy decoding.
    let mut units=Vec::new();
    for n in 0..=maximum {
        let c=unsafe { *pointer.as_ptr().add(n) };
        if c==0 { return String::from_utf16(&units).map_err(|_| "Invalid WebView UTF-16.".into()); }
        if n==maximum { break; } units.push(c);
    }
    Err("WebView string exceeds bound.".into())
}

fn same_sender(sender: Option<ICoreWebView2Environment>, events: &Events) -> bool {
    sender.and_then(|s| s.cast::<IUnknown>().ok()).is_some_and(|s| s.as_raw() as usize==events.environment)
}

pub fn install(window: tauri::WebviewWindow, launch: Launch) -> Result<(),String> {
    let owned=window.clone();
    window.with_webview(move |platform| {
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Life::new(owned.clone(),launch,platform)));
        match result {
            Ok(Ok(life)) => LIFE.with(|slot| { *slot.borrow_mut()=Some(life); }),
            _ => { CALLBACK_FAULT.with(|f|f.set(true)); owned.state::<Bridge>().native_failure(); eprintln!("STOP | WebView admission failed; retain case and window."); },
        }
    }).map_err(|_| "Cannot schedule native WebView admission.".into())
}

impl Life {
    fn new(window: tauri::WebviewWindow, launch: Launch, platform: tauri::webview::PlatformWebview) -> Result<Self,String> {
        let started=Instant::now();
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&launch.native_log)
            .map_err(|_| "Native evidence output already exists or is unavailable.")?;
        let environment=platform.environment();
        let controller = platform.controller();
        let core=com(unsafe { controller.CoreWebView2() })?;
        let mut browser=0; com(unsafe { core.BrowserProcessId(&mut browser) })?;
        if browser==0 { return Err("Missing browser identity.".into()); }
        let flags = Rc::new(Events {
            environment: com(environment.cast::<IUnknown>())?.as_raw() as usize,
            browser,
            changed: Cell::new(true),
            exit: Cell::new(None),
            fault: Cell::new(false),
            shortcut: Cell::new(None),
        });
        let mut life = Self {
            app: window.app_handle().clone(),
            window: Some(window),
            controller: Some(controller),
            env5: com(environment.cast())?,
            env8: com(environment.cast())?,
            flags,
            browser_token: None,
            process_token: None,
            accelerator_token: None,
            timer: 0,
            expected_profile: fs::canonicalize(&launch.profile)
                .map_err(|_| "Cannot identify owned profile.")?,
            expected_version: launch.webview_version,
            log,
            log_bytes: 0,
            started,
            sampled: started,
            closing: None,
            destroy_requested: false,
            destroyed: false,
            failed: false,
        };
        // Partial registrations remain owned in TLS if setup subsequently fails.
        let setup=life.register().and_then(|_|life.sample()).and_then(|_| {
            life.timer=unsafe { SetTimer(None,0,250,Some(timer)) };
            if life.timer==0 { return Err("UI timer creation failed.".into()); }
            life.app.state::<Bridge>().native_ready()
        });
        if let Err(reason)=setup { life.stop(&reason); }
        Ok(life)
    }
    fn register(&mut self) -> Result<(),String> {
        let weak=Rc::downgrade(&self.flags);
        let exited=BrowserProcessExitedEventHandler::create(Box::new(move |sender,args| {
            let Some(events)=weak.upgrade() else { return Ok(()); };
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> windows::core::Result<()> {
                if !same_sender(sender,&events) { events.fault.set(true); return Ok(()); }
                let Some(args)=args else { events.fault.set(true); return Ok(()); };
                let mut pid=0; let mut kind=COREWEBVIEW2_BROWSER_PROCESS_EXIT_KIND_NORMAL;
                unsafe { args.BrowserProcessId(&mut pid)?; args.BrowserProcessExitKind(&mut kind)?; }
                if pid!=events.browser || events.exit.get().is_some() { events.fault.set(true); }
                else { events.exit.set(Some((pid,kind.0))); }
                Ok(())
            }));
            if !matches!(result,Ok(Ok(()))) { events.fault.set(true); }
            Ok(())
        }));
        let mut token=0; com(unsafe { self.env5.add_BrowserProcessExited(&exited,&mut token) })?; self.browser_token=Some(token);
        let weak=Rc::downgrade(&self.flags);
        let changed=ProcessInfosChangedEventHandler::create(Box::new(move |sender,_| {
            if let Some(events)=weak.upgrade() {
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if !same_sender(sender,&events) { events.fault.set(true); } else { events.changed.set(true); }
                }));
                if result.is_err() { events.fault.set(true); }
            }
            Ok(())
        }));
        com(unsafe { self.env8.add_ProcessInfosChanged(&changed,&mut token) })?; self.process_token=Some(token);
        self.register_window_shortcuts()
    }

    fn register_window_shortcuts(&mut self) -> Result<(), String> {
        let controller = self.controller.as_ref().ok_or("Missing WebView controller.")?;
        let weak = Rc::downgrade(&self.flags);
        let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            let Some(events) = weak.upgrade() else { return Ok(()); };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> windows::core::Result<()> {
                    let Some(args) = args else {
                        events.fault.set(true);
                        return Ok(());
                    };
                    let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP;
                    let mut key = 0;
                    unsafe {
                        args.KeyEventKind(&mut kind)?;
                        args.VirtualKey(&mut key)?;
                    }
                    if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                        || !matches!(u8::try_from(key), Ok(b'W' | b'Q'))
                    {
                        return Ok(());
                    }
                    let mut physical = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();
                    unsafe { args.PhysicalKeyStatus(&mut physical)?; }
                    let other_modifier_down = physical.IsMenuKeyDown.as_bool()
                        || key_is_down(VK_MENU) || key_is_down(VK_SHIFT)
                        || key_is_down(VK_LWIN) || key_is_down(VK_RWIN);
                    if let Some(shortcut) =
                        window_shortcut(kind, key, key_is_down(VK_CONTROL), other_modifier_down)
                    {
                        // Set the handled flag before deferring work to the UI timer.
                        unsafe { args.SetHandled(true)?; }
                        if !physical.WasKeyDown.as_bool() && events.shortcut.get().is_none() {
                            events.shortcut.set(Some(shortcut));
                        }
                    }
                    Ok(())
                },
            ));
            if !matches!(result, Ok(Ok(()))) {
                events.fault.set(true);
            }
            Ok(())
        }));
        let mut token = 0;
        com(unsafe { controller.add_AcceleratorKeyPressed(&handler, &mut token) })?;
        self.accelerator_token = Some(token);
        Ok(())
    }

    fn record(&mut self, value: serde_json::Value) -> Result<(),String> {
        let mut bytes=yokai_ipc::encode(&value,64*1024).map_err(|_| "Native observation exceeds bound.")?; bytes.push(b'\n');
        if self.log_bytes+bytes.len()>8*1024*1024 { return Err("Native evidence cap reached.".into()); }
        self.log.write_all(&bytes).and_then(|_|self.log.flush()).map_err(|_| "Native evidence write failed.")?;
        self.log_bytes+=bytes.len(); Ok(())
    }
    fn sample(&mut self) -> Result<(),String> {
        let env7: ICoreWebView2Environment7=com(self.env8.cast())?;
        let mut folder=PWSTR::null(); com(unsafe { env7.UserDataFolder(&mut folder) })?;
        let folder=PathBuf::from(strict_string(folder,32767)?); ordinary(&folder,true)?;
        if fs::canonicalize(&folder).map_err(|_| "Cannot identify actual profile.")?!=self.expected_profile { return Err("Actual WebView profile differs.".into()); }
        let mut version=PWSTR::null(); com(unsafe { self.env8.BrowserVersionString(&mut version) })?;
        let version=strict_string(version,128)?;
        if version!=self.expected_version { return Err("Actual WebView version differs.".into()); }
        let infos=com(unsafe { self.env8.GetProcessInfos() })?;
        let mut count=0; com(unsafe { infos.Count(&mut count) })?;
        if count>15 { return Err("WebView process count exceeds owned desktop bound.".into()); }
        let mut processes=Vec::new(); let mut seen=BTreeSet::new();
        for index in 0..count {
            let p=com(unsafe { infos.GetValueAtIndex(index) })?;
            let mut pid=0; let mut kind=COREWEBVIEW2_PROCESS_KIND_BROWSER;
            com(unsafe { p.ProcessId(&mut pid) })?; com(unsafe { p.Kind(&mut kind) })?;
            if pid<=0 || !seen.insert(pid) || !matches!(kind,COREWEBVIEW2_PROCESS_KIND_BROWSER|COREWEBVIEW2_PROCESS_KIND_RENDERER|COREWEBVIEW2_PROCESS_KIND_UTILITY|COREWEBVIEW2_PROCESS_KIND_SANDBOX_HELPER|COREWEBVIEW2_PROCESS_KIND_GPU|COREWEBVIEW2_PROCESS_KIND_PPAPI_PLUGIN|COREWEBVIEW2_PROCESS_KIND_PPAPI_BROKER) { return Err("Invalid native process facts.".into()); }
            processes.push(ProcessFact { pid,kind:kind.0 });
        }
        if !self.destroy_requested && !processes.iter().any(|p|p.pid==self.flags.browser as i32 && p.kind==COREWEBVIEW2_PROCESS_KIND_BROWSER.0) { return Err("Browser missing from native process facts.".into()); }
        let measured=Instant::now();
        if measured.duration_since(self.sampled)>Duration::from_secs(10) { return Err("Native observation cadence exceeded.".into()); }
        let facts=self.app.state::<Bridge>().facts()?;
        self.record(serde_json::json!({"event":"observation","elapsed_seconds":measured.duration_since(self.started).as_secs_f64(),"profile":folder,"version":version,"browser_pid":self.flags.browser,"processes":processes,"process_infos_changed":self.flags.changed.replace(false),"browser_exit":self.flags.exit.get(),"native_facts":facts,"external_identity_and_job_check_required":true}))?;
        if Instant::now().duration_since(self.sampled)>Duration::from_secs(10) { return Err("Native observation completion exceeded cadence.".into()); }
        self.sampled=measured; Ok(())
    }
    fn stop(&mut self, reason: &str) {
        if !self.failed {
            self.failed=true; self.app.state::<Bridge>().native_failure();
            let _=self.record(serde_json::json!({"event":"STOP","reason":reason,"elapsed_seconds":self.started.elapsed().as_secs_f64(),"preserve_owners":true}));
            eprintln!("STOP | {reason} Retain the case and owners.");
        }
    }
    fn quit(&mut self) {
        if self.closing.is_none() && !self.failed
            && let Err(reason) = self.app.state::<Bridge>().request_quit()
        {
            eprintln!("Cannot quit Yokai: {reason}");
        }
    }

    fn close(&mut self, confirmed_quit_only: bool) -> Result<(), String> {
        if self.closing.is_none()
            && self.app.state::<Bridge>().begin_window_close(confirmed_quit_only)?
        {
            self.closing = Some(Instant::now());
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<(),String> {
        if self.failed { return Ok(()); }
        if CALLBACK_FAULT.with(Cell::get) || self.flags.fault.get() { return Err("Native callback or reentrancy failed.".into()); }
        if self.sampled.elapsed()>Duration::from_secs(10) { return Err("Native observation stale.".into()); }
        match self.flags.shortcut.take() {
            Some(Shortcut::Close) => self.close(false)?,
            Some(Shortcut::Quit) => self.quit(),
            None => (),
        }
        self.close(true)?;
        if self.closing.is_some_and(|t|t.elapsed()>Duration::from_secs(20)) { return Err("Normal host close exceeded 20 seconds.".into()); }
        if let Some((pid,kind))=self.flags.exit.get() {
            if !self.destroy_requested || pid!=self.flags.browser || kind!=COREWEBVIEW2_BROWSER_PROCESS_EXIT_KIND_NORMAL.0 { return Err("Browser exited outside normal controller release.".into()); }
        }
        if self.sampled.elapsed()>=Duration::from_secs(5) { self.sample()?; }
        if self.closing.is_some() && !self.destroy_requested && self.app.state::<Bridge>().join_finished()? {
            if let Some(token) = self.accelerator_token {
                let controller = self.controller.as_ref().ok_or("Missing WebView controller.")?;
                com(unsafe { controller.remove_AcceleratorKeyPressed(token) })?;
                self.accelerator_token = None;
            }
            self.controller = None;
            self.destroy_requested=true;
            if let Some(window)=self.window.take() { window.destroy().map_err(|_| "Controller destruction request failed.")?; }
        }
        if self.destroyed && self.flags.exit.get()==Some((self.flags.browser,COREWEBVIEW2_BROWSER_PROCESS_EXIT_KIND_NORMAL.0)) {
            if let Some(token)=self.process_token { com(unsafe { self.env8.remove_ProcessInfosChanged(token) })?; self.process_token=None; }
            if let Some(token)=self.browser_token { com(unsafe { self.env5.remove_BrowserProcessExited(token) })?; self.browser_token=None; }
            com(unsafe { KillTimer(None,self.timer) })?; self.timer=0;
            // Reuse the existing terminal write: fast connect/close cannot lose its facts
            // between periodic observations. The worker has already joined here.
            let facts=self.app.state::<Bridge>().facts()?;
            facts.authentication.require_complete().map_err(str::to_owned)?;
            self.record(serde_json::json!({"event":"native_close_observed","elapsed_seconds":self.started.elapsed().as_secs_f64(),"runtime_exit_not_inferred":true,"external_host_helper_departure_required":true,"native_facts":facts}))?;
            if self.closing.is_none_or(|t|t.elapsed()>Duration::from_secs(20)) { return Err("Terminal native close bound exceeded.".into()); }
            EXIT_READY.with(|v|v.set(true)); self.app.exit(0);
        }
        Ok(())
    }
}

unsafe extern "system" fn timer(_: HWND, _: u32, id: usize, _: u32) {
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| LIFE.with(|slot| {
        let Ok(mut owner)=slot.try_borrow_mut() else { CALLBACK_FAULT.with(|f|f.set(true)); return; };
        if let Some(life)=owner.as_mut() {
            if id!=life.timer { life.stop("Unexpected timer identity."); return; }
            if let Err(reason)=life.tick() { life.stop(&reason); }
        }
    })));
    if result.is_err() { CALLBACK_FAULT.with(|f|f.set(true)); }
}

pub fn event(event: RunEvent) {
    match event {
        RunEvent::WindowEvent { label,event:WindowEvent::CloseRequested { api,.. },.. } if label=="main" => {
            api.prevent_close(); request_window_close();
        }
        RunEvent::WindowEvent { label,event:WindowEvent::Destroyed,.. } if label=="main" => with_life(|life| {
            if let Err(reason)=controller_destroyed(life.closing.is_some(),life.destroy_requested) { life.stop(reason); }
            else { life.destroyed=true; }
        }),
        RunEvent::ExitRequested { api,.. } => {
            if !EXIT_READY.with(Cell::get) { api.prevent_exit(); request_window_close(); }
        }
        _ => (),
    }
}

pub fn request_quit() {
    with_life(Life::quit);
}

fn request_window_close() {
    with_life(|life| {
        if let Err(reason) = life.close(false) {
            life.stop(&reason);
        }
    });
}

pub fn controller_destroyed(closing:bool,requested:bool) -> Result<(), &'static str> {
    if closing && requested { Ok(()) } else { Err("Unexpected window destruction; normal close not verified.") }
}

fn with_life(f: impl FnOnce(&mut Life)) {
    LIFE.with(|slot| match slot.try_borrow_mut() { Ok(mut owner)=>{if let Some(life)=owner.as_mut() {f(life);}}, Err(_)=>CALLBACK_FAULT.with(|v|v.set(true)) });
}

pub fn returned() -> Result<(),String> {
    if !EXIT_READY.with(Cell::get) { return Err("Native departure was not verified; preserve evidence.".into()); }
    LIFE.with(|slot| { slot.borrow_mut().take(); });
    Ok(())
}

#[cfg(test)]
mod close_shortcut_tests {
    use super::{
        window_shortcut, Shortcut, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
        COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP, COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
    };

    #[test]
    fn focused_webview_routes_exact_control_shortcuts() {
        for (key, expected) in [(b'W', Shortcut::Close), (b'Q', Shortcut::Quit)] {
            let key = u32::from(key);
            assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, key, true, false), Some(expected));
            assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, key, false, false), None);
            assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, key, true, true), None);
            assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP, key, true, false), None);
            assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN, key, true, false), None);
        }
        assert_eq!(window_shortcut(COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, u32::from(b'Z'), true, false), None);
    }
}
